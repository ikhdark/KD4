use super::*;
use crate::state::TaskKind;
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
use codex_protocol::items::AgentMessageContent;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
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
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

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
        config.model_context_window = Some(10_000);
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
        config.model_context_window = Some(10_000);
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
