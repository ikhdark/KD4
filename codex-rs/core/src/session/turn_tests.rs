use super::*;
use crate::session::reasoning_governor::AuthoritativeWaitOwnerResult;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tasks::SessionTaskResult;
use crate::tools::exposure::AgentSurfaceStage;
use crate::tools::exposure::EnvironmentSurfaceMode;
use crate::tools::exposure::GoalSurfaceState;
use crate::tools::exposure::ToolExposureIdentity;
use crate::tools::registry::ToolRegistry;
use crate::tools::router::ToolRouter;
use anyhow::Result;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_extension_api::ExtensionData;
use codex_extension_api::TurnInputContributor;
use codex_extension_api::TurnItemContributor;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::PathUri;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use indexmap::IndexMap;
use pretty_assertions::assert_eq;
use rmcp::model::JsonObject;
use rmcp::model::Tool;
use std::collections::HashMap;
use std::fs;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[test]
fn tool_relay_reconciliation_advances_without_watchdog() {
    let timing = TurnTimingState::default();
    timing.mark_turn_started();
    let mut orphan_passes = 0;

    timing.adjust_parallel_gate_waiters(1);
    assert_eq!(
        reconcile_turn_progress(&timing, 1, &mut orphan_passes),
        NextSampleBlockReason::WaitingForGate
    );
    timing.adjust_parallel_gate_waiters(-1);
    timing.adjust_relay_queue_depth(1);
    assert_eq!(
        reconcile_turn_progress(&timing, 1, &mut orphan_passes),
        NextSampleBlockReason::WaitingForDelivery
    );
    timing.adjust_relay_queue_depth(-1);
    assert_eq!(
        reconcile_turn_progress(&timing, 0, &mut orphan_passes),
        NextSampleBlockReason::ReadyToSample
    );
}

fn authoritative_wait_result(
    surfaceable_message: Option<&str>,
) -> crate::session::reasoning_governor::AuthoritativeWaitOwnerResult {
    crate::session::reasoning_governor::AuthoritativeWaitOwnerResult {
        adapter: "code_mode_cell".to_string(),
        value: serde_json::json!("arbitrary raw execution output"),
        surfaceable_message: surfaceable_message.map(ToOwned::to_owned),
    }
}

fn recommended_plugin_candidate(id: &str, name: &str) -> DiscoverableTool {
    codex_tools::DiscoverablePluginInfo {
        id: id.to_string(),
        remote_plugin_id: None,
        name: name.to_string(),
        description: None,
        has_skills: false,
        mcp_server_names: Vec::new(),
        app_connector_ids: Vec::new(),
    }
    .into()
}

#[test]
fn recommended_plugins_are_not_injected_for_unrelated_tasks() {
    let selected = task_relevant_recommended_plugins(
        &[ContentItem::InputText {
            text: "fix the parser".to_string(),
        }],
        vec![recommended_plugin_candidate("figma", "Figma")],
    );

    assert!(selected.is_empty());
}

#[test]
fn named_recommended_plugin_is_the_only_injected_candidate() {
    let selected = task_relevant_recommended_plugins(
        &[ContentItem::InputText {
            text: "use Figma for this mockup".to_string(),
        }],
        vec![
            recommended_plugin_candidate("figma", "Figma"),
            recommended_plugin_candidate("notion", "Notion"),
        ],
    );

    assert_eq!(
        selected
            .iter()
            .map(DiscoverableTool::name)
            .collect::<Vec<_>>(),
        vec!["Figma"]
    );
}

#[test]
fn generic_plugin_recommendation_request_injects_the_catalog() {
    let selected = task_relevant_recommended_plugins(
        &[ContentItem::InputText {
            text: "suggest a plugin".to_string(),
        }],
        vec![
            recommended_plugin_candidate("figma", "Figma"),
            recommended_plugin_candidate("notion", "Notion"),
        ],
    );

    assert_eq!(selected.len(), 2);
}

#[test]
fn recommended_plugin_catalog_is_bounded_by_rendered_bytes() {
    let candidates = (0..50)
        .map(|index| {
            recommended_plugin_candidate(
                &format!("plugin-{index}-{}", "x".repeat(180)),
                &format!("Plugin {index} {}", "y".repeat(180)),
            )
        })
        .collect();
    let instructions = RecommendedPluginsInstructions::from_plugins(candidates)
        .expect("at least one bounded plugin entry");
    let ResponseItem::Message { content, .. } = ContextualUserFragment::into(instructions) else {
        panic!("expected recommended plugin message");
    };
    let ContentItem::InputText { text } = &content[0] else {
        panic!("expected recommended plugin text");
    };

    assert!(text.len() <= 4_160);
    assert!(!text.contains("Plugin 49"));
}

#[test]
fn proven_loop_terminal_generation_ends_unless_new_input_arrives() {
    let request = GenerationRequestDisposition {
        purpose: Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning),
        sampling: SamplingGenerationDisposition::DecisionBearing,
        relevant_state_fingerprint: "state".to_string(),
        failure_fingerprint: None,
        terminal_completion_only: true,
    };

    assert!(!generation_needs_follow_up(&request, true, false));
    assert!(generation_needs_follow_up(&request, false, true));
}

#[test]
fn authoritative_wait_terminal_surface_requires_explicit_owner_projection() {
    let without_projection = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            authoritative_wait_result(None),
        )),
        ..Default::default()
    };
    assert_eq!(
        authoritative_wait_terminal_surface(&without_projection),
        Some(SurfacedToolResult {
            adapter: "code_mode_cell".to_string(),
            value: serde_json::json!("arbitrary raw execution output"),
            canonical_message: None,
        }),
        "raw code-mode output must not become last_agent_message"
    );

    let with_projection = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            authoritative_wait_result(Some("owner-designated completion")),
        )),
        ..Default::default()
    };
    assert_eq!(
        authoritative_wait_terminal_surface(&with_projection),
        Some(SurfacedToolResult {
            adapter: "code_mode_cell".to_string(),
            value: serde_json::json!("arbitrary raw execution output"),
            canonical_message: Some("owner-designated completion".to_string()),
        })
    );
}

#[test]
fn blocked_authoritative_wait_never_enters_terminal_surface() {
    let blocked = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::ModelRequired,
        authoritative_wait: Some(AuthoritativeWaitResolution::Blocked(
            authoritative_wait_result(Some("must not surface")),
        )),
        ..Default::default()
    };
    assert_eq!(authoritative_wait_terminal_surface(&blocked), None);
}

fn run_turn_multi_thread_test_with_stack<F, Fut, T>(test_name: &'static str, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = T> + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(test_name.to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(8 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("turn test runtime")
                .block_on(test())
        })
        .expect("turn test thread")
        .join()
        .expect("turn test thread panicked")
}

struct RewriteAgentMessageContributor;

struct TurnInputBudgetContributor {
    text: String,
}

struct EnvironmentEchoContributor;

struct CountingTurnInputContributor {
    poll_count: Arc<AtomicUsize>,
}

struct ExposureOnlyTool {
    name: &'static str,
    exposure: codex_extension_api::ToolExposure,
}

impl codex_extension_api::ToolExecutor<codex_extension_api::ToolCall> for ExposureOnlyTool {
    fn tool_name(&self) -> codex_extension_api::ToolName {
        codex_extension_api::ToolName::plain(self.name)
    }

    fn spec(&self) -> codex_extension_api::ToolSpec {
        panic!("exposure identity tests do not build tool schemas")
    }

    fn exposure(&self) -> codex_extension_api::ToolExposure {
        self.exposure
    }

    fn handle(
        &self,
        _call: codex_extension_api::ToolCall,
    ) -> codex_extension_api::ToolExecutorFuture<'_> {
        Box::pin(async { panic!("exposure identity tests do not dispatch tools") })
    }
}

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
        Ok(crate::tasks::TurnTaskResult::default())
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

impl TurnInputContributor for TurnInputBudgetContributor {
    fn contribute<'a>(
        &'a self,
        _input: TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        Box::pin(async move {
            vec![
                Box::new(codex_context_fragments::RenderedContextFragment::new(
                    "user",
                    self.text.clone(),
                )) as Box<dyn codex_extension_api::ContextualUserFragment + Send>,
            ]
        })
    }
}

impl TurnInputContributor for EnvironmentEchoContributor {
    fn contribute<'a>(
        &'a self,
        input: TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        Box::pin(async move {
            let environment = input
                .environments
                .first()
                .expect("primary turn environment should reach contributors");
            vec![
                Box::new(codex_context_fragments::RenderedContextFragment::new(
                    "user",
                    format!(
                        "extension-environment:{}:{}:{}",
                        environment.environment_id, environment.cwd, environment.is_primary
                    ),
                )) as Box<dyn codex_extension_api::ContextualUserFragment + Send>,
            ]
        })
    }
}

impl TurnInputContributor for CountingTurnInputContributor {
    fn contribute<'a>(
        &'a self,
        _input: TurnInputContext,
        _session_store: &'a ExtensionData,
        _thread_store: &'a ExtensionData,
        _turn_store: &'a ExtensionData,
    ) -> codex_extension_api::ExtensionFuture<
        'a,
        Vec<Box<dyn codex_extension_api::ContextualUserFragment + Send>>,
    > {
        self.poll_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Vec::new() })
    }
}

fn assistant_output_text(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: Some(ResponseItemId::with_suffix("msg", "1")),
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn ordinary_continuation_precedence_is_stable() {
    assert_eq!(
        ordinary_continuation_cause(true, true, true),
        Some(ContinuationCause::ToolResult)
    );
    assert_eq!(
        ordinary_continuation_cause(false, true, true),
        Some(ContinuationCause::ServerEndTurnFalse)
    );
    assert_eq!(
        ordinary_continuation_cause(false, false, true),
        Some(ContinuationCause::PendingInput)
    );
    assert_eq!(ordinary_continuation_cause(false, false, false), None);
}

#[test]
fn finalized_router_reuse_requires_identical_coarse_exposure_identity() {
    let disabled = ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Disabled,
        environment_mode: EnvironmentSurfaceMode::None,
        ..ToolExposureIdentity::default()
    };
    let router = ToolRouter::from_parts_with_warnings_and_identity(
        ToolRegistry::empty_for_test(),
        Vec::new(),
        Vec::new(),
        disabled.clone(),
    );

    assert!(finalized_router_matches_exposure(&router, &disabled));

    let inactive = ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Inactive,
        ..disabled.clone()
    };
    assert!(!finalized_router_matches_exposure(&router, &inactive));

    let active = ToolExposureIdentity {
        goal_surface_state: GoalSurfaceState::Active,
        ..disabled.clone()
    };
    assert!(!finalized_router_matches_exposure(&router, &active));

    let ready_environment = ToolExposureIdentity {
        environment_mode: EnvironmentSurfaceMode::One,
        ..disabled.clone()
    };
    assert!(!finalized_router_matches_exposure(
        &router,
        &ready_environment
    ));

    let starting_environment = ToolExposureIdentity {
        environment_starting: true,
        ..disabled
    };
    assert!(!finalized_router_matches_exposure(
        &router,
        &starting_environment
    ));
}

#[test]
fn goal_surface_state_has_disabled_inactive_and_active_transitions() {
    let tool = |name, exposure| {
        Arc::new(ExposureOnlyTool { name, exposure })
            as Arc<dyn codex_extension_api::ToolExecutor<codex_extension_api::ToolCall>>
    };

    assert_eq!(goal_surface_state(&[]), GoalSurfaceState::Disabled);
    assert_eq!(
        goal_surface_state(&[tool(
            "create_goal",
            codex_extension_api::ToolExposure::Deferred,
        )]),
        GoalSurfaceState::Inactive
    );
    assert_eq!(
        goal_surface_state(&[
            tool("create_goal", codex_extension_api::ToolExposure::Deferred,),
            tool("get_goal", codex_extension_api::ToolExposure::Direct),
            tool("update_goal", codex_extension_api::ToolExposure::Direct),
        ]),
        GoalSurfaceState::Active
    );
}

#[test]
fn agent_surface_stage_depends_only_on_coarse_graph_and_binding_state() {
    assert_eq!(
        agent_surface_stage_from_snapshot(false, false, false),
        AgentSurfaceStage::Prohibited
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, false, false),
        AgentSurfaceStage::SpawnOnly
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(false, true, false),
        AgentSurfaceStage::Lifecycle
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(false, false, true),
        AgentSurfaceStage::TypedAdministration
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, true, false),
        AgentSurfaceStage::Lifecycle
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, false, true),
        AgentSurfaceStage::TypedAdministration
    );
    assert_eq!(
        agent_surface_stage_from_snapshot(true, true, true),
        AgentSurfaceStage::TypedAdministration
    );

    // Running/waiting status, gates, targets, and capacity are deliberately absent from this
    // snapshot, so those fine-grained transitions cannot change the schema identity.
    assert_eq!(
        agent_surface_stage_from_snapshot(true, true, false),
        agent_surface_stage_from_snapshot(true, true, false)
    );
}

fn response_input_texts(items: &[ResponseItem]) -> Vec<&str> {
    let mut texts = Vec::new();
    for item in items {
        if let ResponseItem::Message { content, .. } = item {
            for content_item in content {
                if let ContentItem::InputText { text } = content_item {
                    texts.push(text.as_str());
                }
            }
        }
    }
    texts
}

#[test]
fn reasoning_governor_resets_only_for_non_empty_user_input() {
    let user_input = TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "new instruction".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    };
    let empty_user_input = TurnInput::UserInput {
        content: Vec::new(),
        client_id: None,
    };
    let response_item = TurnInput::ResponseItem(assistant_output_text("context"));
    let mailbox_item = TurnInput::InterAgentCommunication(InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "mailbox context".to_string(),
        true,
    ));

    assert!(resets_reasoning_governor(&user_input));
    assert!(!resets_reasoning_governor(&empty_user_input));
    assert!(!resets_reasoning_governor(&response_item));
    assert!(!resets_reasoning_governor(&mailbox_item));
}

#[test]
fn legacy_explicit_skill_items_share_one_hard_budget() {
    let max_bytes = codex_utils_string::approx_bytes_for_tokens(
        codex_context_fragments::MAX_MODEL_CONTEXT_TOKENS,
    );
    let items = build_bounded_skill_context_items([
        (
            "user",
            format!("legacy-skill-budget-first:{}", "x".repeat(max_bytes)),
        ),
        ("user", "legacy-skill-budget-second".to_string()),
    ]);
    let texts = response_input_texts(&items);

    assert!(texts.iter().map(|text| text.len()).sum::<usize>() <= max_bytes);
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("legacy-skill-budget-first:"))
    );
    assert!(
        texts
            .iter()
            .all(|text| !text.contains("legacy-skill-budget-second"))
    );
}

#[tokio::test]
async fn extension_turn_input_contributors_share_one_hard_budget() {
    let max_bytes = codex_utils_string::approx_bytes_for_tokens(
        codex_context_fragments::MAX_MODEL_CONTEXT_TOKENS,
    );
    let (mut session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_input_contributor(Arc::new(TurnInputBudgetContributor {
        text: format!("turn-input-budget-first:{}", "x".repeat(max_bytes)),
    }));
    builder.turn_input_contributor(Arc::new(TurnInputBudgetContributor {
        text: "turn-input-budget-second".to_string(),
    }));
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let step_context = StepContext::for_test(Arc::new(turn_context));

    let items =
        build_extension_turn_input_items(&session, &step_context, &[], &CancellationToken::new())
            .await
            .expect("turn-input contributors should render");
    let texts = response_input_texts(&items);

    assert!(texts.iter().map(|text| text.len()).sum::<usize>() <= max_bytes);
    assert!(
        texts
            .iter()
            .any(|text| text.starts_with("turn-input-budget-first:"))
    );
    assert!(
        texts
            .iter()
            .all(|text| !text.contains("turn-input-budget-second"))
    );
}

#[tokio::test]
async fn extension_turn_input_contributors_receive_foreign_environment_uris() {
    #[cfg(unix)]
    let foreign_cwd = PathUri::parse("file:///C:/workspace").expect("Windows cwd URI");
    #[cfg(windows)]
    let foreign_cwd = PathUri::parse("file:///usr/local/project").expect("POSIX cwd URI");
    assert!(
        foreign_cwd.to_abs_path().is_err(),
        "test cwd must be foreign to the host"
    );

    let (mut session, mut turn_context) = crate::session::tests::make_session_and_context().await;
    let environment = turn_context.environments.turn_environments[0].clone();
    turn_context.environments.turn_environments[0] =
        crate::session::turn_context::TurnEnvironment::new(
            "remote".to_string(),
            environment.environment,
            foreign_cwd.clone(),
            environment.shell,
        );
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::new();
    builder.turn_input_contributor(Arc::new(EnvironmentEchoContributor));
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let step_context = StepContext::for_test(Arc::new(turn_context));

    let items =
        build_extension_turn_input_items(&session, &step_context, &[], &CancellationToken::new())
            .await
            .expect("foreign environment should render through extension context");
    let texts = response_input_texts(&items);

    assert_eq!(
        texts,
        vec![format!("extension-environment:remote:{foreign_cwd}:true")]
    );
}

#[test]
fn streamed_item_with_empty_id_gets_a_generated_id() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "streamed_item_with_empty_id_gets_a_generated_id",
        streamed_item_with_empty_id_gets_a_generated_id_impl,
    )
}

async fn streamed_item_with_empty_id_gets_a_generated_id_impl() -> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_config(|config| {
            let _ = config.features.enable(Feature::ItemIds);
        })
        .build(&server)
        .await?;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("response-1"),
            responses::ev_message_item_added("", ""),
            responses::ev_output_text_delta("streamed"),
            responses::ev_assistant_message("", "streamed"),
            responses::ev_completed("response-1"),
        ]),
    )
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "stream a response".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    let started_id = core_test_support::wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemStarted(event) => match &event.item {
            TurnItem::AgentMessage(item) => Some(item.id.clone()),
            _ => None,
        },
        _ => None,
    })
    .await;
    let completed_id = core_test_support::wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemCompleted(event) => match &event.item {
            TurnItem::AgentMessage(item) => Some(item.id.clone()),
            _ => None,
        },
        _ => None,
    })
    .await;

    assert!(started_id.starts_with("msg_"));
    assert_eq!(started_id, completed_id);
    response_mock.single_request();
    Ok(())
}

fn non_openai_model_provider(server: &wiremock::MockServer) -> ModelProviderInfo {
    let mut provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    provider.name = "OpenAI (phase 68 test)".to_string();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    provider
}

fn complete_compaction_summary(state: &str) -> String {
    format!(
        "## Goal\nresume the pending turn\n\n## Current state\n{state}\n\n## Completed work\nseed turn completed\n\n## Unresolved work\npending input remains\n\n## Evidence\nmock compaction response\n\n## Next action\nsample the pending input"
    )
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

#[tokio::test]
async fn drain_in_flight_returns_first_error_after_draining_remaining_futures() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let remaining_future_polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let remaining_future_polled_clone = Arc::clone(&remaining_future_polled);
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "first",
            Box::pin(async { Err(CodexErr::Fatal("first tool failure".to_string())) }),
        )
        .into_future(),
    ));
    in_flight.push_back(Box::pin(
        InFlightToolCall::from_test_future(
            "second",
            Box::pin(async move {
                remaining_future_polled_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                Err(CodexErr::Fatal("second tool failure".to_string()))
            }),
        )
        .into_future(),
    ));

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

#[test]
fn initial_response_item_triggers_compaction_before_the_stream_request() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "initial_response_item_triggers_compaction_before_the_stream_request",
        initial_response_item_triggers_compaction_before_the_stream_request_impl,
    )
}

async fn initial_response_item_triggers_compaction_before_the_stream_request_impl() -> Result<()> {
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
                    /*total_tokens*/ 21_000,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("response-item-compact-response"),
                responses::ev_assistant_message(
                    "response-item-compact-message",
                    &complete_compaction_summary("initial response context compacted"),
                ),
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
        config.model_context_window = Some(100_000);
        config.model_auto_compact_token_limit = Some(22_000);
        config.model_auto_compact_token_limit_scope =
            codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
        config.model_provider.request_max_retries = Some(0);
        config.model_provider.stream_max_retries = Some(0);
        let _ = config.features.disable(Feature::RemoteCompactionV2);
    });
    let test = builder.build(&server).await?;

    test.submit_turn("seed committed history near the compaction limit")
        .await?;
    while tokio::time::timeout(Duration::from_millis(10), test.codex.next_event())
        .await
        .is_ok()
    {}
    test.codex
        .submit(Op::UserInput {
            items: Vec::new(),
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([(
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
        while request_log.requests().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the final sampling request should follow pre-turn compaction");
    let submitted_turn_id = request_log.requests()[2].body_json()["client_metadata"]["turn_id"]
        .as_str()
        .expect("final request turn id")
        .to_string();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match test.codex.next_event().await.expect("turn event").msg {
                EventMsg::Error(error) => {
                    panic!(
                        "response-item turn failed during pre-turn compaction: {}",
                        error.message
                    )
                }
                EventMsg::TurnComplete(turn) if submitted_turn_id == turn.turn_id => {
                    assert!(
                        turn.error.is_none(),
                        "response-item turn completed with an error: {:?}",
                        turn.error
                    );
                    break;
                }
                _ => {}
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

#[test]
fn oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit",
        oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit_impl,
    )
}

async fn oversized_pending_input_compacts_once_when_committed_history_is_also_over_limit_impl()
-> Result<()> {
    core_test_support::skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let request_log = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("seed-response"),
                responses::ev_assistant_message("seed-message", "seed complete"),
                responses::ev_completed_with_tokens("seed-response", /*total_tokens*/ 23_000),
            ]),
            responses::sse(vec![
                responses::ev_response_created("compact-response"),
                responses::ev_assistant_message(
                    "compact-message",
                    &complete_compaction_summary("oversized pending input compacted"),
                ),
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
    let pending_plan_builds = Arc::new(AtomicUsize::new(0));
    let mut extension_builder = codex_extension_api::ExtensionRegistryBuilder::new();
    extension_builder.turn_input_contributor(Arc::new(CountingTurnInputContributor {
        poll_count: Arc::clone(&pending_plan_builds),
    }));
    let mut builder = test_codex()
        .with_extensions(Arc::new(extension_builder.build()))
        .with_config(move |config| {
            config.model_provider = provider;
            config.model_context_window = Some(100_000);
            config.model_auto_compact_token_limit = Some(22_000);
            config.model_auto_compact_token_limit_scope =
                codex_protocol::config_types::AutoCompactTokenLimitScope::Total;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            let _ = config.features.disable(Feature::RemoteCompactionV2);
        });
    let test = builder.build(&server).await?;

    test.submit_turn("seed committed history").await?;
    while tokio::time::timeout(Duration::from_millis(10), test.codex.next_event())
        .await
        .is_ok()
    {}
    pending_plan_builds.store(0, Ordering::SeqCst);
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "oversized pending payload ".repeat(128),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while request_log.requests().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the final sampling request should follow pending-input compaction");
    let submitted_turn_id = request_log.requests()[2].body_json()["client_metadata"]["turn_id"]
        .as_str()
        .expect("final request turn id")
        .to_string();
    loop {
        match test.codex.next_event().await.expect("turn event").msg {
            EventMsg::Error(error) => {
                panic!(
                    "oversized pending-input turn failed during compaction: {}",
                    error.message
                )
            }
            EventMsg::TurnComplete(turn) if submitted_turn_id == turn.turn_id => {
                assert!(
                    turn.error.is_none(),
                    "oversized pending-input turn completed with an error: {:?}",
                    turn.error
                );
                break;
            }
            _ => {}
        }
    }

    assert_eq!(
        request_log.requests().len(),
        3,
        "the second turn should compact once, then sample instead of repeatedly compacting the same pending payload"
    );
    assert_eq!(
        pending_plan_builds.load(Ordering::SeqCst),
        2,
        "compaction should invalidate the initial pure projection and rebuild the pending plan against compacted history"
    );
    Ok(())
}

#[test]
fn pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot",
        pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot_impl,
    )
}

async fn pending_plan_and_router_reuse_one_step_mcp_inventory_snapshot_impl() -> Result<()> {
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
    let planning_generation = session.services.planning_generation();
    let PendingTurnPlanBuild::Ready(plan) = build_pure_pending_turn_plan(
        &session,
        Arc::clone(&step_context),
        &input,
        planning_generation,
        &cancellation_token,
    )
    .await?
    else {
        panic!("stable test inputs should produce a ready pending-turn plan");
    };
    assert!(
        plan.projected_prompt_pressure.total_tokens
            > estimate_pending_tokens(
                &input,
                &[],
                &[],
                plan.first_router.as_ref(),
                /*initial_context*/ true,
            )
            .total_tokens,
        "first-turn planning must account for full context before compaction"
    );
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

fn wait_for_concurrent_state_attempt(attempted: &std::sync::atomic::AtomicBool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !attempted.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        attempted.load(Ordering::Acquire),
        "concurrent state attempt did not start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_plan_commit_and_invalidation_share_the_session_state_owner() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    let stale_generation = session.services.planning_generation();
    let history_before = session.clone_history().await.into_raw_items();
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );

    // Invalidation owns SessionState first. The pending commit cannot perform
    // its final comparison until after generation N has been invalidated.
    let commit = {
        let mut state_owner = session.state.lock().await;
        let commit_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let commit_attempted_task = Arc::clone(&commit_attempted);
        let commit_session = Arc::clone(&session);
        let commit_step_context = Arc::clone(&step_context);
        let commit = tokio::spawn(async move {
            commit_attempted_task.store(true, Ordering::Release);
            commit_session
                .compare_and_record_context_updates(commit_step_context.as_ref(), stale_generation)
                .await
        });
        wait_for_concurrent_state_attempt(&commit_attempted);
        assert!(!commit.is_finished());
        session
            .services
            .advance_planning_generation(&mut state_owner);
        commit
    };
    assert!(commit.await.expect("commit task completed").is_none());
    assert_eq!(
        session.clone_history().await.into_raw_items(),
        history_before
    );
    assert!(
        session
            .state
            .lock()
            .await
            .pending_context_baseline()
            .is_none()
    );

    // Prepare a real context candidate, then replay the exact synchronous
    // compare-and-commit primitive while retaining SessionState ownership.
    let current_generation = session.services.planning_generation();
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    assert!(
        session
            .compare_and_record_context_updates(step_context.as_ref(), current_generation)
            .await
            .is_some()
    );
    let invalidation = {
        let mut state_owner = session.state.lock().await;
        let candidate = state_owner
            .pending_context_baseline()
            .expect("successful context commit stages its baseline");
        state_owner.clear_pending_context_baseline();
        let marker = ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: Vec::new(),
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };
        let history_len_before_commit = state_owner.clone_history().into_raw_items().len();
        let invalidation_attempted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invalidation_attempted_task = Arc::clone(&invalidation_attempted);
        let invalidation_session = Arc::clone(&session);
        let invalidation = tokio::spawn(async move {
            invalidation_attempted_task.store(true, Ordering::Release);
            let mut state_owner = invalidation_session.state.lock().await;
            invalidation_session
                .services
                .advance_planning_generation(&mut state_owner)
        });
        wait_for_concurrent_state_attempt(&invalidation_attempted);
        assert!(!invalidation.is_finished());

        assert!(
            session
                .compare_and_commit_planning_state(
                    &mut state_owner,
                    Some(current_generation),
                    |state| {
                        state.record_items(
                            std::iter::once(&marker),
                            turn_context.model_info.truncation_policy.into(),
                        );
                        state.stage_context_baseline(candidate);
                    },
                )
                .is_some()
        );
        assert_eq!(
            state_owner.clone_history().into_raw_items().len(),
            history_len_before_commit + 1
        );
        assert!(state_owner.pending_context_baseline().is_some());
        assert!(!invalidation.is_finished());
        invalidation
    };

    let next_generation = invalidation.await.expect("invalidation task completed");
    assert!(next_generation > current_generation);
}

#[tokio::test]
async fn realized_context_commits_only_the_bound_physical_attempt() {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    session
        .record_context_updates_and_set_reference_context_item(step_context.as_ref())
        .await;

    assert!(
        session
            .bind_context_baseline_candidate("request-1", "attempt-1")
            .await
    );
    assert!(
        session
            .bind_context_baseline_candidate("request-1", "attempt-2")
            .await
    );
    assert!(
        !session
            .commit_context_baseline_candidate("request-1", "attempt-1")
            .await
            .expect("stale attempt must be ignored")
    );
    assert!(session.reference_context_item().await.is_none());

    assert!(
        session
            .commit_context_baseline_candidate("request-1", "attempt-2")
            .await
            .expect("matching attempt commits")
    );
    let realized = session
        .reference_context_item()
        .await
        .expect("matching accepted attempt realizes context");
    let provenance = realized
        .context_provenance
        .expect("realized context records accepted attempt provenance");
    assert_eq!(provenance.accepted_attempt.sampling_request_id, "request-1");
    assert_eq!(provenance.accepted_attempt.physical_attempt_id, "attempt-2");
    assert!(!provenance.fragment_digests.is_empty());
    assert!(
        !session
            .commit_context_baseline_candidate("request-1", "attempt-2")
            .await
            .expect("duplicate Created must be ignored")
    );
}

#[tokio::test]
async fn pending_plan_rebuilds_after_generation_changes_during_planning() -> Result<()> {
    let (session, turn_context, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let history_before = session.clone_history().await.into_raw_items();
    let planning_generation = session.services.planning_generation();
    let stale_step_context = session
        .capture_step_context(Arc::clone(&turn_context))
        .await;
    {
        let mut state_owner = session.state.lock().await;
        session
            .services
            .advance_planning_generation(&mut state_owner);
    }

    let build = build_pure_pending_turn_plan(
        &session,
        stale_step_context,
        &[],
        planning_generation,
        &CancellationToken::new(),
    )
    .await?;
    assert!(matches!(build, PendingTurnPlanBuild::Stale));
    assert_eq!(
        session.clone_history().await.into_raw_items(),
        history_before
    );

    let rebuilt_generation = session.services.planning_generation();
    let rebuilt_step_context = session.capture_step_context(turn_context).await;
    let rebuilt = build_pure_pending_turn_plan(
        &session,
        rebuilt_step_context,
        &[],
        rebuilt_generation,
        &CancellationToken::new(),
    )
    .await?;
    let PendingTurnPlanBuild::Ready(rebuilt) = rebuilt else {
        panic!("a plan rebuilt from the current generation should be ready");
    };
    assert_eq!(rebuilt.planning_generation, rebuilt_generation);
    Ok(())
}

#[test]
fn pending_token_estimate_includes_model_visible_tool_schemas() {
    let empty_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let empty_router = ToolRouter::from_parts(empty_registry, Vec::new());
    let schema_description = "schema context ".repeat(1024);
    let schema_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let schema_router = ToolRouter::from_parts(
        schema_registry,
        vec![codex_tools::ToolSpec::Function(
            codex_tools::ResponsesApiTool {
                name: "large_schema_tool".to_string(),
                description: schema_description,
                strict: false,
                defer_loading: None,
                parameters: codex_tools::JsonSchema::object(
                    Default::default(),
                    None,
                    Some(false.into()),
                ),
                output_schema: None,
            },
        )],
    );

    let baseline =
        estimate_pending_tokens(&[], &[], &[], &empty_router, /*initial_context*/ false);
    let with_schema = estimate_pending_tokens(
        &[],
        &[],
        &[],
        &schema_router,
        /*initial_context*/ false,
    );

    assert!(
        with_schema.total_tokens > baseline.total_tokens + 3_000,
        "model-visible schema bytes must materially increase pre-turn context estimation"
    );
    assert_eq!(
        with_schema.body_growth_tokens, baseline.body_growth_tokens,
        "stable tool schemas must not count as body-after-prefix growth"
    );
}

#[test]
fn pending_token_estimate_excludes_stable_startup_injections_from_body_growth() {
    let empty_registry = crate::tools::registry::ToolRegistry::from_tools(std::iter::empty::<
        Arc<dyn crate::tools::registry::CoreToolRuntime>,
    >());
    let empty_router = ToolRouter::from_parts(empty_registry, Vec::new());
    let baseline =
        estimate_pending_tokens(&[], &[], &[], &empty_router, /*initial_context*/ false);
    let guidance = ContextualUserFragment::into(TaskModelGuidance);
    let with_guidance = estimate_pending_tokens(
        &[],
        &[guidance],
        &[],
        &empty_router,
        /*initial_context*/ false,
    );

    assert!(with_guidance.total_tokens > baseline.total_tokens);
    assert_eq!(
        with_guidance.body_growth_tokens,
        baseline.body_growth_tokens
    );
}

#[test]
fn stop_hook_continuation_preserves_finalization_warning_for_the_final_response() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "stop_hook_continuation_preserves_finalization_warning_for_the_final_response",
        stop_hook_continuation_preserves_finalization_warning_for_the_final_response_impl,
    )
}

async fn stop_hook_continuation_preserves_finalization_warning_for_the_final_response_impl()
-> Result<()> {
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
                            "status": "implemented",
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

#[test]
fn models_etag_refresh_does_not_block_stream_events_and_is_cancellable() -> Result<()> {
    run_turn_multi_thread_test_with_stack(
        "models_etag_refresh_does_not_block_stream_events_and_is_cancellable",
        models_etag_refresh_does_not_block_stream_events_and_is_cancellable_impl,
    )
}

async fn models_etag_refresh_does_not_block_stream_events_and_is_cancellable_impl() -> Result<()> {
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
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if matches!(
                test.codex
                    .next_event()
                    .await
                    .expect("cancellation event")
                    .msg,
                EventMsg::TurnAborted(_)
            ) {
                break;
            }
        }
    })
    .await
    .expect("interrupt should cancel the delayed models refresh promptly");
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
    let mut last_agent_message = None;
    let item = assistant_output_text("original assistant text");

    let handled = handle_assistant_item_done_in_plan_mode(
        &session,
        &turn_context,
        &turn_store,
        &item,
        &mut state,
        /*previously_active_item*/ None,
        &mut last_agent_message,
    )
    .await;

    assert!(handled);
    assert_eq!(
        last_agent_message.as_deref(),
        Some("plan contributed assistant text")
    );
}

fn synthetic_tool_result(call_id: &str) -> ResponseInputItem {
    ResponseInputItem::ToolSearchOutput {
        call_id: call_id.to_string(),
        status: "completed".to_string(),
        execution: "client".to_string(),
        tools: Vec::new(),
        omitted_result_count: None,
    }
}

fn controlled_tool_future(
    call_id: &'static str,
    first_poll: tokio::sync::oneshot::Sender<tokio::time::Instant>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> BoxFuture<'static, CodexResult<ResponseInputItem>> {
    Box::pin(async move {
        let _ = first_poll.send(tokio::time::Instant::now());
        let _ = release.await;
        Ok(synthetic_tool_result(call_id))
    })
}

fn controlled_tool_call(
    call_id: &'static str,
    first_poll: tokio::sync::oneshot::Sender<tokio::time::Instant>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> InFlightToolCall {
    InFlightToolCall::from_test_future(
        call_id,
        controlled_tool_future(call_id, first_poll, release),
    )
}

#[tokio::test(start_paused = true)]
async fn eager_tool_poll_overlaps_a_controlled_response_tail_without_changing_results() {
    const RESPONSE_TAIL: Duration = Duration::from_millis(250);
    let baseline_item_accepted = tokio::time::Instant::now();
    let (baseline_first_poll_tx, mut baseline_first_poll_rx) = tokio::sync::oneshot::channel();
    let (baseline_release_tx, baseline_release_rx) = tokio::sync::oneshot::channel();
    let mut baseline: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> =
        FuturesOrdered::new();
    baseline.push_back(controlled_tool_future(
        "read-1",
        baseline_first_poll_tx,
        baseline_release_rx,
    ));

    tokio::task::yield_now().await;
    assert!(matches!(
        baseline_first_poll_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    tokio::time::advance(RESPONSE_TAIL).await;
    let baseline_result_task =
        tokio::spawn(async move { baseline.next().await.expect("baseline result should exist") });
    let baseline_first_poll = baseline_first_poll_rx
        .await
        .expect("baseline future should be polled");
    assert_eq!(
        baseline_first_poll.duration_since(baseline_item_accepted),
        RESPONSE_TAIL
    );
    baseline_release_tx
        .send(())
        .expect("baseline tool should still be attached");
    let baseline_result = baseline_result_task
        .await
        .expect("baseline result task should finish")
        .expect("baseline tool should succeed");

    let eager_item_accepted = tokio::time::Instant::now();
    let (eager_first_poll_tx, eager_first_poll_rx) = tokio::sync::oneshot::channel();
    let (eager_release_tx, eager_release_rx) = tokio::sync::oneshot::channel();
    let mut eager: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> = FuturesOrdered::new();
    eager.push_back(start_eager_tool_future(controlled_tool_call(
        "read-1",
        eager_first_poll_tx,
        eager_release_rx,
    )));

    let eager_first_poll = eager_first_poll_rx
        .await
        .expect("eager future should start before the response tail is released");
    assert_eq!(
        eager_first_poll.duration_since(eager_item_accepted),
        Duration::ZERO
    );
    let (stream_tail_completed_tx, stream_tail_completed_rx) = tokio::sync::oneshot::channel();
    let stream_tail = tokio::spawn(async move {
        tokio::time::sleep(RESPONSE_TAIL).await;
        let _ = stream_tail_completed_tx.send(tokio::time::Instant::now());
    });
    tokio::task::yield_now().await;
    tokio::time::advance(RESPONSE_TAIL).await;
    let eager_tail_completed = stream_tail_completed_rx
        .await
        .expect("the model-response tail should continue while the tool is blocked");
    stream_tail.await.expect("stream-tail task should finish");
    assert_eq!(
        eager_tail_completed.duration_since(eager_first_poll),
        RESPONSE_TAIL
    );

    // The simulated stream tail completed while tool work was still deliberately blocked.
    // Model continuation remains behind the ordered tool-result barrier.
    let next_sampling_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let next_sampling_started_after_drain = Arc::clone(&next_sampling_started);
    let continuation = tokio::spawn(async move {
        let result = eager
            .next()
            .await
            .expect("eager result should exist")
            .result
            .expect("eager tool should succeed");
        next_sampling_started_after_drain.store(true, Ordering::SeqCst);
        result
    });
    tokio::task::yield_now().await;
    assert!(!next_sampling_started.load(Ordering::SeqCst));
    eager_release_tx
        .send(())
        .expect("eager tool should still be attached");
    let eager_result = continuation
        .await
        .expect("continuation barrier task should finish");

    assert_eq!(eager_result, baseline_result);
    assert!(next_sampling_started.load(Ordering::SeqCst));
}

#[tokio::test]
async fn eager_tool_results_remain_in_call_order_after_reverse_completion() {
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
    let (first_release_tx, first_release_rx) = tokio::sync::oneshot::channel();
    let (second_started_tx, second_started_rx) = tokio::sync::oneshot::channel();
    let (second_release_tx, second_release_rx) = tokio::sync::oneshot::channel();
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(controlled_tool_call(
        "first",
        first_started_tx,
        first_release_rx,
    )));
    in_flight.push_back(start_eager_tool_future(controlled_tool_call(
        "second",
        second_started_tx,
        second_release_rx,
    )));

    first_started_rx.await.expect("first tool should start");
    second_started_rx.await.expect("second tool should start");
    second_release_tx
        .send(())
        .expect("second tool should still be attached");
    tokio::task::yield_now().await;
    first_release_tx
        .send(())
        .expect("first tool should still be attached");

    let first = in_flight
        .next()
        .await
        .expect("first result")
        .result
        .expect("success");
    let second = in_flight
        .next()
        .await
        .expect("second result")
        .result
        .expect("success");
    assert_eq!(first, synthetic_tool_result("first"));
    assert_eq!(second, synthetic_tool_result("second"));
}

#[tokio::test(start_paused = true)]
async fn eager_tool_failure_is_observed_only_after_response_streaming_finishes() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let future: BoxFuture<'static, CodexResult<ResponseInputItem>> = Box::pin(async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
        Err(CodexErr::Fatal("synthetic eager tool failure".to_string()))
    });
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(InFlightToolCall::from_test_future(
        "failure", future,
    )));

    started_rx.await.expect("eager tool should start");
    tokio::time::advance(Duration::from_millis(250)).await;
    let response_tail_finished = true;
    assert!(response_tail_finished);

    release_tx
        .send(())
        .expect("failed eager tool should remain attached until collection drain");
    let error = in_flight
        .next()
        .await
        .expect("failed result should retain its ordered slot")
        .result
        .expect_err("synthetic tool should fail");
    assert!(error.to_string().contains("synthetic eager tool failure"));
}

struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

#[tokio::test]
async fn dropping_in_flight_collection_aborts_eager_tool_work() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
    let future: BoxFuture<'static, CodexResult<ResponseInputItem>> = Box::pin(async move {
        let _drop_signal = DropSignal(Some(dropped_tx));
        let _ = started_tx.send(());
        std::future::pending::<()>().await;
        unreachable!("aborted eager work must not resume")
    });
    let mut in_flight: FuturesOrdered<BoxFuture<'static, InFlightToolResult>> =
        FuturesOrdered::new();
    in_flight.push_back(start_eager_tool_future(InFlightToolCall::from_test_future(
        "drop", future,
    )));

    started_rx.await.expect("eager work should start");
    drop(in_flight);
    dropped_rx
        .await
        .expect("dropping the collection must abort, not detach, eager work");
}
#[test]
fn terminal_surface_preserves_typed_owner_result_without_json_message_synthesis() {
    let value = serde_json::json!({"status": "complete", "nested": {"answer": 42}});
    let decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            AuthoritativeWaitOwnerResult {
                adapter: "owner_adapter".to_string(),
                value: value.clone(),
                surfaceable_message: None,
            },
        )),
        ..Default::default()
    };

    let surfaced = authoritative_wait_terminal_surface(&decision).expect("typed surface");
    assert_eq!(surfaced.adapter, "owner_adapter");
    assert_eq!(surfaced.value, value);
    assert_eq!(surfaced.canonical_message, None);
}

#[test]
fn terminal_surface_preserves_owner_canonical_message_exactly() {
    let canonical = "  owner-authored completion\n";
    let decision = SamplingConvergenceDecision {
        continuation: ContinuationDisposition::SurfaceExistingResult,
        authoritative_wait: Some(AuthoritativeWaitResolution::Terminal(
            AuthoritativeWaitOwnerResult {
                adapter: "owner_adapter".to_string(),
                value: serde_json::json!({"message": "different structured value"}),
                surfaceable_message: Some(canonical.to_string()),
            },
        )),
        ..Default::default()
    };

    let surfaced = authoritative_wait_terminal_surface(&decision).expect("typed surface");
    assert_eq!(surfaced.canonical_message.as_deref(), Some(canonical));
}

#[test]
fn projected_prompt_pressure_does_not_add_stable_tools_to_server_usage_twice() {
    assert_eq!(
        projected_prompt_tokens_from_estimates(
            /*active_context_tokens*/ 900, /*committed_history_tokens*/ 500,
            /*pending_token_estimate*/ 450,
        ),
        950
    );
    assert_eq!(
        projected_prompt_tokens_from_estimates(
            /*active_context_tokens*/ 1_200, /*committed_history_tokens*/ 500,
            /*pending_token_estimate*/ 450,
        ),
        1_200
    );
}

#[test]
fn plan_mode_memory_citations_are_parsed_once_for_live_events() {
    let mut state = PlanModeStreamState::new("turn-1");
    let raw = "<citation_entries>\nMEMORY.md:1-2|note=[x]\n</citation_entries>\n<rollout_ids>\n019cc2ea-1dff-7902-8d40-c8f6e5d83cc4\n</rollout_ids>";

    let citation =
        take_new_memory_citation(&mut state, vec![raw.to_string()]).expect("valid memory citation");
    assert_eq!(citation.entries.len(), 1);
    assert_eq!(citation.entries[0].path, "MEMORY.md");
    assert_eq!(
        citation.rollout_ids,
        vec!["019cc2ea-1dff-7902-8d40-c8f6e5d83cc4"]
    );
    assert_eq!(
        take_new_memory_citation(&mut state, vec![raw.to_string()]),
        None
    );
}
