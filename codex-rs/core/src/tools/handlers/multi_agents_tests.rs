use super::*;
use crate::FunctionCallError;
use crate::ThreadManager;
use crate::agent::control::AgentControlTestBarrier;
use crate::config::AgentRoleConfig;
use crate::init_state_db;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context as make_session_and_context_base;
use crate::session::tests::make_session_and_context_with_rx as make_session_and_context_with_rx_base;
use crate::session::turn_context::TurnContext;
use crate::test_harness::TempDirExt;
use crate::tools::context::ToolOutput;
use crate::tools::handlers::multi_agents_v2::FollowupTaskHandler as FollowupTaskHandlerV2;
use crate::tools::handlers::multi_agents_v2::InterruptAgentHandler;
use crate::tools::handlers::multi_agents_v2::ListAgentsHandler as ListAgentsHandlerV2;
use crate::tools::handlers::multi_agents_v2::SendMessageHandler as SendMessageHandlerV2;
use crate::tools::handlers::multi_agents_v2::SpawnAgentHandler as SpawnAgentHandlerV2;
use crate::tools::handlers::multi_agents_v2::WaitAgentHandler as WaitAgentHandlerV2;
use crate::turn_diff_tracker::TurnDiffTracker;
use crate::windows_sandbox::WindowsSandboxLevelExt;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_5_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID;
use codex_model_provider_info::AMAZON_BEDROCK_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ServiceTier;
use codex_protocol::config_types::ShellEnvironmentPolicy;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::models::SandboxEnforcement;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::FileSystemAccessMode;
use codex_protocol::protocol::FileSystemPath;
use codex_protocol::protocol::FileSystemSandboxEntry;
use codex_protocol::protocol::FileSystemSandboxPolicy;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::NetworkSandboxPolicy;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SandboxPolicy;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

async fn make_session_and_context() -> (crate::session::Session, TurnContext) {
    let (mut session, turn) = make_session_and_context_base().await;
    session.services.models_manager = turn.provider.models_manager(
        &turn.config.model_provider_id,
        turn.config.codex_home.to_path_buf(),
        Some(
            codex_models_manager::test_support::test_models_response().expect("test model catalog"),
        ),
    );
    (session, turn)
}

async fn make_session_and_context_with_rx() -> (
    Arc<crate::session::Session>,
    Arc<TurnContext>,
    async_channel::Receiver<codex_protocol::protocol::Event>,
) {
    let (mut session, turn, events) = make_session_and_context_with_rx_base().await;
    Arc::get_mut(&mut session)
        .expect("unique fixture session")
        .services
        .models_manager = turn.provider.models_manager(
        &turn.config.model_provider_id,
        turn.config.codex_home.to_path_buf(),
        Some(
            codex_models_manager::test_support::test_models_response().expect("test model catalog"),
        ),
    );
    (session, turn, events)
}

struct RemoveFileOnDrop(Option<PathBuf>);

impl RemoveFileOnDrop {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for RemoveFileOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn invocation(
    session: Arc<crate::session::session::Session>,
    turn: Arc<TurnContext>,
    tool_name: &str,
    payload: ToolPayload,
) -> ToolInvocation {
    let step_context = StepContext::for_test(Arc::clone(&turn));
    ToolInvocation {
        session,
        step_context,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "call-1".to_string(),
        tool_name: codex_tools::ToolName::plain(tool_name),
        source: crate::tools::context::ToolCallSource::Direct,
        payload,
    }
}

fn function_payload(args: serde_json::Value) -> ToolPayload {
    ToolPayload::Function {
        arguments: args.to_string(),
    }
}

async fn completed_collab_item(
    rx: &async_channel::Receiver<codex_protocol::protocol::Event>,
    call_id: &str,
) -> codex_protocol::items::CollabAgentToolCallItem {
    timeout(Duration::from_secs(1), async {
        loop {
            let event = rx.recv().await.expect("collaboration lifecycle event");
            if let EventMsg::ItemCompleted(event) = event.msg
                && let codex_protocol::items::TurnItem::CollabAgentToolCall(item) = event.item
                && item.id == call_id
            {
                break item;
            }
        }
    })
    .await
    .expect("completed collaboration item should arrive")
}

fn parse_agent_id(id: &str) -> ThreadId {
    ThreadId::from_string(id).expect("agent id should be valid")
}

fn thread_manager() -> ThreadManager {
    ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        built_in_model_providers(/* openai_base_url */ /*openai_base_url*/ None)["openai"].clone(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    )
}

/// Production derives a turn's service tier from the session configuration
/// when the turn starts; spawns read the session's live value.
async fn set_session_service_tier(session: &crate::session::session::Session, tier: &str) {
    session
        .update_settings(crate::session::SessionSettingsUpdate {
            service_tier: Some(Some(tier.to_string())),
            ..Default::default()
        })
        .await
        .expect("service tier update");
}

async fn install_role_with_model_override(turn: &mut TurnContext) -> String {
    let role_name = "fork-context-role".to_string();
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home should be created");
    let role_config_path = turn
        .config
        .codex_home
        .as_path()
        .join("fork-context-role.toml");
    tokio::fs::write(
        &role_config_path,
        r#"model = "gpt-5-role-override"
model_provider = "ollama"
model_reasoning_effort = "minimal"
"#,
    )
    .await
    .expect("role config should be written");

    let mut config = (*turn.config).clone();
    config.agent_roles.insert(
        role_name.clone(),
        AgentRoleConfig {
            description: Some("Role with model overrides".to_string()),
            config_file: Some(role_config_path),
            nickname_candidates: None,
            config_snapshot: Default::default(),
        },
    );
    turn.config = Arc::new(config);

    role_name
}

fn set_turn_config(turn: &mut TurnContext, config: crate::config::Config) {
    turn.model_info.supports_search_tool = false;
    turn.multi_agent_version = config.multi_agent_version_from_features();
    turn.config = Arc::new(config);
}

fn expect_text_output<T>(output: T) -> (String, Option<bool>)
where
    T: ToolOutput,
{
    let response = output.to_response_item(
        "call-1",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );
    match response {
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => {
            let content = match output.body {
                FunctionCallOutputBody::Text(text) => text,
                FunctionCallOutputBody::ContentItems(items) => {
                    codex_protocol::models::function_call_output_content_items_to_text(&items)
                        .unwrap_or_default()
                }
            };
            (content, output.success)
        }
        other => panic!("expected function output, got {other:?}"),
    }
}

#[derive(Debug, Deserialize)]
struct ListAgentsResult {
    agents: Vec<ListedAgentResult>,
}

#[derive(Debug, Deserialize)]
struct ListedAgentResult {
    agent_name: String,
    runtime_loaded: bool,
    agent_status: serde_json::Value,
    last_task_message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InterruptAgentResult {
    previous_status: AgentStatus,
}

#[tokio::test]
async fn handler_rejects_non_function_payloads() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        ToolPayload::Custom {
            input: "hello".to_string(),
        },
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("payload should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "collab handler received unsupported payload".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_rejects_empty_message() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"task_name": "test_agent","message": "   "})),
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("empty message should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("Empty message can't be sent to an agent".to_string())
    );
}



#[tokio::test]
async fn spawn_agent_is_generic_and_preserves_approval_policy() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager.start_thread((*turn.config).clone()).await.expect("start root");
    session.thread_id = root.thread_id;
    session.services.agent_control = manager.agent_control();
    let mut config = (*turn.config).clone();
    let provider_info =
        built_in_model_providers(/* openai_base_url */ /*openai_base_url*/ None)["ollama"].clone();
    config.model_provider_id = "ollama".to_string();
    config.model_provider = provider_info.clone();
    config
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    turn.provider = create_model_provider(provider_info, turn.auth_manager.clone());
    turn.config = Arc::new(config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"task_name": "test_agent",
            "message": "inspect this repo"
        })),
    );
    let output = SpawnAgentHandlerV2::default()
        .handle(invocation)
        .await
        .expect("spawn_agent should succeed");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let agent_id = spawned_thread_id(&manager, &result.task_name).await;
    assert_eq!(result.nickname, None);
    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(snapshot.approval_policy, AskForApproval::OnRequest);
    assert_eq!(snapshot.model_provider_id, "ollama");
}

#[tokio::test]
async fn spawn_agent_keeps_effective_bedrock_model_when_sol_6_1_is_unavailable() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    let (mut session, turn) = make_session_and_context().await;
    let provider_info = ModelProviderInfo::create_amazon_bedrock_provider(/*aws*/ None);
    let provider = create_model_provider(provider_info.clone(), turn.auth_manager.clone());
    session.services.models_manager = provider.models_manager(
        &turn.config.model_provider_id,
        turn.config.codex_home.to_path_buf(),
        /*config_model_catalog*/ None,
    );
    let mut turn = turn
        .with_model(
            AMAZON_BEDROCK_GPT_5_5_MODEL_ID.to_string(),
            &session.services.models_manager,
        )
        .await;
    let mut config = (*turn.config).clone();
    config.model_provider_id = AMAZON_BEDROCK_PROVIDER_ID.to_string();
    config.model_provider = provider_info.clone();
    turn.provider = provider;
    turn.config = Arc::new(config);

    let manager = ThreadManager::with_models_provider_for_tests(
        CodexAuth::from_api_key("dummy"),
        provider_info,
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({"task_name": "test_agent","message": "inspect this repo"})),
        ))
        .await
        .expect("spawn_agent should resolve the Bedrock-qualified default model");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let snapshot = manager
        .get_thread(spawned_thread_id(&manager, &result.task_name).await)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(snapshot.model, AMAZON_BEDROCK_GPT_5_5_MODEL_ID);
    assert_eq!(
        snapshot.reasoning_effort,
        Some(DEFAULT_SPAWN_AGENT_REASONING_EFFORT)
    );
}

#[tokio::test]
async fn spawn_agent_ignores_configured_role_model_and_provider() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home should be created");
    let role_config_path = turn.config.codex_home.join("bedrock-role.toml");
    tokio::fs::write(
        &role_config_path,
        format!(
            "model_provider = \"{AMAZON_BEDROCK_PROVIDER_ID}\"\nmodel = \"{AMAZON_BEDROCK_GPT_5_6_SOL_MODEL_ID}\"\n"
        ),
    )
    .await
    .expect("Bedrock role config should be written");
    let mut config = (*turn.config).clone();
    config.agent_roles.insert(
        "bedrock".to_string(),
        AgentRoleConfig {
            description: Some("Bedrock model role".to_string()),
            config_file: Some(role_config_path.to_path_buf()),
            nickname_candidates: None,
            config_snapshot: Default::default(),
        },
    );
    set_turn_config(&mut turn, config);
    let expected_provider_id = turn.config.model_provider_id.clone();

    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({"task_name": "test_agent",
                "message": "inspect this repo"
            })),
        ))
        .await
        .expect("spawn_agent should validate defaults against the role provider");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let snapshot = manager
        .get_thread(spawned_thread_id(&manager, &result.task_name).await)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(snapshot.model_provider_id, expected_provider_id);
    assert_eq!(snapshot.model, DEFAULT_SPAWN_AGENT_MODEL);
    assert_eq!(
        snapshot.reasoning_effort,
        Some(DEFAULT_SPAWN_AGENT_REASONING_EFFORT)
    );
}

#[tokio::test]
async fn spawn_agent_rejects_role_selection_before_model_overrides() {
    let (session, mut turn) = make_session_and_context().await;
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home should be created");
    let role_config_path = turn.config.codex_home.join("ultra-role.toml");
    tokio::fs::write(
        &role_config_path,
        r#"model_reasoning_effort = "minimal"
"#,
    )
    .await
    .expect("reasoning-only role config should be written");
    let mut config = (*turn.config).clone();
    config.agent_roles.insert(
        "ultra-role".to_string(),
        AgentRoleConfig {
            description: Some("Role that locks reasoning only".to_string()),
            config_file: Some(role_config_path.to_path_buf()),
            nickname_candidates: None,
            config_snapshot: Default::default(),
        },
    );
    set_turn_config(&mut turn, config);

    let result = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({"task_name": "test_agent",
                "message": "inspect this repo",
                "agent_type": "ultra-role",
                "model": "gpt-5.6-luna"
            })),
        ))
        .await;

    assert_eq!(
        result.err(),
        Some(FunctionCallError::RespondToModel(
            "spawn_agent: specialized roles are no longer supported; put the complete task, scope, restrictions, and evidence requirements in message"
                .to_string()
        ))
    );
}

#[tokio::test]
async fn spawn_agent_events_report_uniform_model_and_reasoning() {
    let (mut session, mut turn, rx) = make_session_and_context_with_rx().await;
    let _role_name = install_role_with_model_override(
        Arc::get_mut(&mut turn).expect("turn context should be uniquely owned"),
    )
    .await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    {
        let session = Arc::get_mut(&mut session).expect("session should be uniquely owned");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;
    }

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "spawn_agent",
            function_payload(json!({"task_name": "test_agent",
                "message": "inspect this repo"
            })),
        ))
        .await
        .expect("spawn should succeed");

    let (started, completed) = timeout(Duration::from_secs(1), async {
        let mut started = None;
        let mut completed = None;
        while started.is_none() || completed.is_none() {
            let event = rx.recv().await.expect("spawn lifecycle event");
            match event.msg {
                EventMsg::ItemStarted(event) => {
                    if let codex_protocol::items::TurnItem::CollabAgentToolCall(item) = event.item
                        && item.id == "call-1"
                    {
                        started = Some(item);
                    }
                }
                EventMsg::ItemCompleted(event) => {
                    if let codex_protocol::items::TurnItem::CollabAgentToolCall(item) = event.item
                        && item.id == "call-1"
                    {
                        completed = Some(item);
                    }
                }
                _ => {}
            }
        }
        (
            started.expect("started spawn item"),
            completed.expect("completed spawn item"),
        )
    })
    .await
    .expect("spawn lifecycle events should arrive");

    assert_eq!(started.model.as_deref(), Some(DEFAULT_SPAWN_AGENT_MODEL));
    assert_eq!(started.reasoning_effort, Some(DEFAULT_SPAWN_AGENT_REASONING_EFFORT));
    assert_eq!(completed.model.as_deref(), Some(DEFAULT_SPAWN_AGENT_MODEL));
    assert_eq!(completed.reasoning_effort, Some(DEFAULT_SPAWN_AGENT_REASONING_EFFORT));
}





#[tokio::test]
async fn multi_agent_v2_spawn_does_not_require_wording_authorization() {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.multi_agent_v2.multi_agent_mode_hint_text = None;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    let manager = thread_manager();
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    set_turn_config(&mut turn, config);
    turn.multi_agent_version = MultiAgentVersion::V2;
    // No user-text grant or synthetic authorization state is installed.

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "wording_independent"
            })),
        ))
        .await
        .expect("spawn must pass durable admission without a parsed user-text grant");
    let (content, _) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn result should be json");
    assert_eq!(result["task_name"], "/root/wording_independent");
    assert!(result["assignment_id"].is_string());
    assert!(
        manager
            .captured_ops()
            .iter()
            .any(|(thread_id, _)| *thread_id != root.thread_id),
        "the child must actually receive its task"
    );
    manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
}

#[tokio::test]
async fn multi_agent_v2_spawn_fork_turns_all_rejects_agent_type_override() {
    let (mut session, mut turn) = make_session_and_context().await;
    let role_name = install_role_with_model_override(&mut turn).await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.allow_full_history_forks = true;
    let turn = TurnContext {
        config: Arc::new(config),
        multi_agent_version: codex_protocol::protocol::MultiAgentVersion::V2,
        ..turn
    };

    let err = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "fork_context_v2",
                "agent_type": role_name,
                "fork_turns": "all"
            })),
        ))
        .await
        .err()
        .expect("fork_turns=all should reject agent_type overrides");

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "spawn_agent: specialized roles are no longer supported; put the complete task, scope, restrictions, and evidence requirements in message".to_string(),
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_rejects_model_and_reasoning_overrides() {
    for overrides in [
        json!({"model": "gpt-5.4"}),
        json!({"reasoning_effort": "low"}),
        json!({"model": "gpt-5.4", "reasoning_effort": "low"}),
    ] {
        let (session, turn) = make_session_and_context().await;
        let mut args = json!({"task_name": "generic", "message": "inspect this repo"});
        args.as_object_mut().unwrap().extend(overrides.as_object().unwrap().clone());
        let error = SpawnAgentHandlerV2::default()
            .handle(invocation(Arc::new(session), Arc::new(turn), "spawn_agent", function_payload(args)))
            .await.err().expect("uniform defaults cannot be overridden");
        assert!(error.to_string().contains("model and reasoning overrides are not supported"));
        assert!(error.to_string().contains("gpt-6.1-sol with xhigh reasoning"));
    }
}

#[test_case::test_case(true; "compact")]
#[test_case::test_case(false; "explicit")]
#[tokio::test]
async fn multi_agent_v2_typed_spawn_persists_and_binds_assignment_before_start(compact: bool) {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    let workspace = tempfile::tempdir().expect("isolated typed-task workspace");
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(workspace.path())
            .status()
            .expect("initialize typed-task repository")
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "--quiet",
                "-m",
                "Initial test workspace",
            ])
            .current_dir(workspace.path())
            .status()
            .expect("create typed-task repository HEAD")
            .success()
    );
    config.cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(workspace.path())
        .expect("absolute typed-task workspace");
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.ephemeral = false;
    let state_runtime = init_state_db(&config)
        .await
        .expect("typed spawn requires persistent test state");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_runtime.clone()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = root.thread.codex.session.services.agent_control.clone();
    session.services.state_db = root.thread.codex.session.services.state_db.clone();
    session.thread_id = root.thread_id;
    let root_session_id = session.services.agent_control.session_id().to_string();
    let agent_control = session.services.agent_control.clone();
    // Session startup binds the validation budget, so the root's store is already
    // open; typed assignments are still created only on request.
    assert_eq!(
        agent_control.task_coordinator().root_session_id(),
        Some(root_session_id.clone())
    );
    assert!(
        agent_control
            .task_coordinator()
            .bindings_for_recovery()
            .is_empty(),
        "a fresh root should defer typed assignments until one is requested"
    );
    let child_config = config.clone();
    let environment = turn
        .environments
        .primary()
        .expect("selected test environment");
    turn.environments.turn_environments = vec![crate::session::turn_context::TurnEnvironment::new(
        environment.environment_id.clone(),
        Arc::clone(&environment.environment),
        codex_utils_path_uri::PathUri::from_abs_path(&config.cwd),
        environment.shell.clone(),
    )];
    set_turn_config(&mut turn, config);
    let task_name = format!(
        "typed_worker_{}",
        ThreadId::new().to_string().replace('-', "")
    );
    let risk_path = format!("typed-risk-{task_name}.txt");
    let repo_root = codex_git_utils::get_git_repo_root(child_config.cwd.as_path())
        .unwrap_or_else(|| child_config.cwd.to_path_buf());
    let risk_file = repo_root.join(&risk_path);
    let mut risk_file_cleanup = RemoveFileOnDrop::new(risk_file.clone());

    let before_initial_submission = Arc::new(AgentControlTestBarrier::default());
    agent_control
        .set_before_initial_submission_barrier(Some(Arc::clone(&before_initial_submission)));
    let spawn_invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "task_name": task_name.clone(),
            "assignment": {
                "objective": "inspect the bounded path",
                "acceptance_criteria": [{
                    "id": "criterion-1",
                    "text": "report evidence"
                }],
                "write_scope": [{"path": risk_path.clone(), "recursive": false}],
                "risk_hints": [format!("path:{risk_path}")],
                "required_evidence": ["focused validation"],
                "stop_condition": "stop after reporting evidence"
            }
        })),
    );
    let spawn_task = tokio::spawn(async move {
        SpawnAgentHandlerV2::default()
            .handle(spawn_invocation)
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        before_initial_submission.wait_until_reached(),
    )
    .await
    .expect("typed spawn should reach initial submission");
    agent_control.set_before_initial_submission_barrier(None);
    let agent_path = AgentPath::root()
        .join(&task_name)
        .expect("typed worker path should be valid");
    let binding = agent_control
        .task_coordinator()
        .binding_for_agent_path(&agent_path)
        .expect("typed assignment should be bound before spawn returns");
    let assignment_id = binding.assignment_id;
    assert_eq!(binding.agent_path, format!("/root/{task_name}"));
    assert!(binding.thread_id.is_some());
    let task = agent_control
        .task_coordinator()
        .get_agent_task(assignment_id, Some(0))
        .await
        .expect("typed assignment should be readable from durable store");
    assert_eq!(
        task.assignment.role,
        codex_agent_task_store::AgentRole::Worker
    );
    assert_eq!(
        task.assignment.integration_plan,
        codex_agent_task_store::IntegrationPlan::SingleWriter
    );
    assert_eq!(task.current_attempt.attempt_id, binding.attempt_id);

    let task_store = agent_control
        .task_coordinator()
        .store()
        .expect("typed task store should remain available");
    std::fs::write(&risk_file, "high-risk evidence\n").expect("high-risk file should change");
    assert_eq!(
        task_store
            .get_agent_task(assignment_id, Some(0))
            .await
            .expect("risk hints should remain durable")
            .assignment
            .risk_hints,
        vec![format!("path:{risk_path}")]
    );

    let child_thread_id = ThreadId::from_string(
        binding
            .thread_id
            .as_deref()
            .expect("typed binding should retain the child thread id"),
    )
    .expect("typed binding thread id should parse");
    let runtime_child = manager.get_thread(child_thread_id).await.unwrap();
    let runtime_turn = runtime_child.codex.session.new_default_turn().await;
    let lifecycle = runtime_turn
        .developer_instructions
        .as_deref()
        .unwrap_or_default();
    assert!(lifecycle.contains("Before your final response, call submit_agent_receipt"));
    assert!(lifecycle.contains("report missing validation honestly"));
    let child_source = agent_control
        .get_agent_config_snapshot(child_thread_id)
        .await
        .expect("spawned typed agent should have a config snapshot")
        .session_source;
    let validation_call_id = format!("validation-{}", ThreadId::new());
    let mut validation_call = codex_agent_task_store::ValidationCall {
        call_id: validation_call_id.clone(),
        attempt_id: binding.attempt_id,
        command_summary: "focused validation".to_string(),
        evidence: codex_agent_task_store::ValidationEvidence::default(),
        status: codex_agent_task_store::ValidationCallStatus::Running,
        recorded_at: chrono::Utc::now(),
    };
    task_store
        .record_validation_call(validation_call.clone())
        .await
        .expect("validation call should start for the bound attempt");
    validation_call.evidence.validation_result = Some(json!({
        "argv": ["focused validation"],
        "coveredPaths": [risk_path.clone()],
        "callId": validation_call_id.clone(),
        "processId": null,
        "status": "succeeded",
        "durationMs": 1,
    }));
    validation_call.status = codex_agent_task_store::ValidationCallStatus::Succeeded;
    validation_call.recorded_at += chrono::Duration::milliseconds(1);
    task_store
        .record_validation_call(validation_call)
        .await
        .expect("validation call should finish for the same bound attempt");
    let proof = task_store
        .get_validation_call(validation_call_id.clone())
        .await
        .expect("stored validation reads")
        .expect("stored validation exists");
    let evidence_ref = json!({
        "call_id": validation_call_id,
        "workspace_id": task.assignment.workspace_id,
        "evidence_epoch": proof.evidence.end_epoch.unwrap(),
        "kind": "validation_execution",
    });
    let (mut child_session, mut child_turn) = make_session_and_context().await;
    child_session.services.agent_control = agent_control.clone();
    child_session.thread_id = child_thread_id;
    child_turn.session_source = child_source;
    set_turn_config(&mut child_turn, child_config);
    let child_session = Arc::new(child_session);
    let child_turn = Arc::new(child_turn);
    let observation_output = crate::tools::handlers::multi_agents_v2::GetAgentTaskHandler
        .handle(invocation(
            Arc::clone(&child_session),
            Arc::clone(&child_turn),
            "get_agent_task",
            function_payload(json!({
                "assignment_id": assignment_id.to_string(),
                "observation_limit": 101
            })),
        ))
        .await
        .expect("oversized observation reads are capped before reaching the store");
    let (observation_text, _) = expect_text_output(observation_output);
    assert!(observation_text.contains(&assignment_id.to_string()));

    let mut receipt_args = json!({
        "status": "completed",
        "summary": "reported bounded evidence",
        "criterion_results": [{
            "criterion_id": "criterion-1",
            "status": "passed",
            "evidence_ref": evidence_ref.clone()
        }],
        "declared_changes": [{
            "path": risk_path.clone(),
            "summary": "recorded high-risk evidence"
        }],
        "validation_call_ids": [validation_call_id.clone()],
        "blockers": [],
        "risks": []
    });
    if compact {
        receipt_args["criterion_results"][0]["evidence_ref"] =
            json!({"call_id": validation_call_id});
        for key in ["validation_call_ids", "blockers", "risks", "next_action"] {
            receipt_args.as_object_mut().unwrap().remove(key);
        }
        // Defaults must not repair an explicitly incorrect claim or accept an unknown call.
        for (field, invalid_value, expected_error) in [
            (
                "workspace_id",
                json!("wrong-workspace"),
                "must reference its workspace",
            ),
            (
                "evidence_epoch",
                json!(proof.evidence.end_epoch.unwrap() + 1),
                "recorded epoch",
            ),
            ("call_id", json!("unknown-call"), "no recorded end epoch"),
        ] {
            let mut invalid_args = receipt_args.clone();
            invalid_args["criterion_results"][0]["evidence_ref"][field] = invalid_value;
            let error = crate::tools::handlers::multi_agents_v2::SubmitAgentReceiptHandler
                .handle(invocation(
                    Arc::clone(&child_session),
                    Arc::clone(&child_turn),
                    "submit_agent_receipt",
                    function_payload(invalid_args),
                ))
                .await
                .err()
                .expect("incorrect evidence must be rejected");
            assert!(error.to_string().contains(expected_error), "{error}");
            let unsealed = task_store
                .get_agent_task(assignment_id, Some(0))
                .await
                .unwrap();
            assert!(unsealed.receipt.is_none());
            assert_eq!(
                unsealed.current_attempt.state,
                codex_agent_task_store::AttemptState::Active
            );
        }
        let mut no_selected_calls = receipt_args.clone();
        no_selected_calls["validation_call_ids"] = json!([]);
        let error = crate::tools::handlers::multi_agents_v2::SubmitAgentReceiptHandler
            .handle(invocation(
                Arc::clone(&child_session),
                Arc::clone(&child_turn),
                "submit_agent_receipt",
                function_payload(no_selected_calls),
            ))
            .await
            .err()
            .expect("an explicit empty call list must not be replaced by inferred calls");
        assert!(
            error.to_string().contains("must reference its workspace"),
            "{error}"
        );
        assert!(
            task_store
                .get_agent_task(assignment_id, Some(0))
                .await
                .unwrap()
                .receipt
                .is_none()
        );
    }
    let codex_tools::ToolSpec::Function(receipt_spec) =
        crate::tools::handlers::multi_agents_v2::SubmitAgentReceiptHandler.spec()
    else {
        panic!("receipt tool must expose its argument schema");
    };
    let schema = serde_json::to_value(receipt_spec.parameters).unwrap();
    assert!(
        jsonschema::validator_for(&schema)
            .unwrap()
            .is_valid(&receipt_args)
    );
    let receipt_invocation = invocation(
        child_session,
        child_turn,
        "submit_agent_receipt",
        function_payload(receipt_args),
    );
    let receipt_router = Arc::new(crate::tools::router::ToolRouter::from_context(
        receipt_invocation.step_context.as_ref(),
        crate::tools::router::ToolRouterParams {
            tool_suggest_candidates: None,
            deferred_mcp_tools: None,
            mcp_tools: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &Default::default(),
    ));
    let receipt_names = receipt_router
        .registered_tool_names_for_test()
        .into_iter()
        .filter(|name| name.name == "submit_agent_receipt")
        .collect::<Vec<_>>();
    assert_eq!(
        receipt_names.len(),
        1,
        "typed child must expose the receipt tool exactly once"
    );
    assert!(
        receipt_invocation
            .step_context
            .set_tool_router(receipt_router)
            .is_ok()
    );
    let receipt_runtime = crate::tools::parallel::ToolCallRuntime::new(
        receipt_invocation.session,
        receipt_invocation.step_context,
        receipt_invocation.tracker,
    );
    let receipt_call = crate::tools::router::ToolCall {
        tool_name: receipt_names[0].clone(),
        call_id: receipt_invocation.call_id,
        payload: receipt_invocation.payload,
    };
    let receipt_task = tokio::spawn(async move {
        receipt_runtime
            .handle_tool_call(receipt_call, receipt_invocation.cancellation_token)
            .await
    });
    let receipt_response = receipt_task
        .await
        .expect("receipt submission task should join")
        .expect("bound attempt should seal a validation-backed risk-gated receipt");
    let ResponseInputItem::FunctionCallOutput { output, .. } = receipt_response else {
        panic!("registered receipt tool must return its result");
    };
    assert_eq!(output.success, Some(true));
    let FunctionCallOutputBody::Text(receipt_text) = &output.body else {
        panic!("receipt response must include the evidence projection");
    };
    // A bounded receipt fits inline without a separate projection envelope.
    let receipt_header: serde_json::Value =
        serde_json::from_str(receipt_text).expect("receipt result JSON");
    assert_eq!(
        receipt_header["completion_evidence"]["criterion_counts"]["with_execution_reference"],
        1
    );
    assert_eq!(
        receipt_header["completion_evidence"]["criterion_counts"]["reported_passed_without_execution_reference"],
        0
    );
    before_initial_submission.release_one();
    let output = spawn_task
        .await
        .expect("typed spawn task should join")
        .expect("typed spawn should create and bind durable task state");
    let (content, _) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert_eq!(result["task_name"], format!("/root/{task_name}"));
    let assignment_id_text = assignment_id.to_string();
    assert_eq!(
        result["assignment_id"].as_str(),
        Some(assignment_id_text.as_str())
    );
    assert_eq!(result["integration_plan"], "single_writer");
    let task_capsule_input = manager
        .captured_ops()
        .into_iter()
        .find_map(|(id, op)| match op {
            Op::UserInput { items, .. } if id == child_thread_id => {
                items.into_iter().find_map(|item| match item {
                    UserInput::Text { text, .. }
                        if text.starts_with("<task_capsule_v1>")
                            && text.ends_with("</task_capsule_v1>") =>
                    {
                        Some(text)
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("typed assignment should use a TaskCapsule as its sole initial input");
    let task_capsule_payload = task_capsule_input
        .strip_prefix("<task_capsule_v1>")
        .and_then(|text| text.strip_suffix("</task_capsule_v1>"))
        .expect("TaskCapsule markers should enclose the canonical payload");
    let task_capsule: serde_json::Value =
        serde_json::from_str(task_capsule_payload).expect("TaskCapsule payload should be JSON");
    assert_eq!(
        task_capsule["assignment_id"].as_str(),
        Some(assignment_id_text.as_str())
    );
    assert_eq!(task_capsule["integration_plan"], "single_writer");
    let _ = agent_control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("typed child should stop after its durable receipt is sealed");

    let reloaded = crate::agent::task_coordinator::AgentTaskCoordinator::default();
    reloaded
        .initialize(state_runtime, root_session_id)
        .await
        .expect("typed task coordinator should reload persisted bindings");
    assert_eq!(
        reloaded
            .binding_for_assignment(assignment_id)
            .expect("reloaded coordinator should restore the typed binding"),
        binding
    );
    let reloaded_task = reloaded
        .get_agent_task(assignment_id, Some(0))
        .await
        .expect("reloaded coordinator should restore the typed task lifecycle");
    assert_eq!(
        reloaded_task.current_attempt.state,
        codex_agent_task_store::AttemptState::Completed
    );
    let sealed = reloaded_task.receipt.as_ref().unwrap();
    assert!(sealed.blockers.is_empty());
    assert!(sealed.risks.is_empty());
    assert_eq!(sealed.next_action, None);
    assert_eq!(
        serde_json::to_value(
            reloaded_task.receipt.as_ref().unwrap().criterion_results[0]
                .evidence_ref
                .as_ref()
        )
        .unwrap(),
        evidence_ref
    );
    assert_eq!(
        reloaded_task
            .receipt
            .expect("reloaded task should retain its sealed receipt")
            .validation_call_ids,
        vec![validation_call_id]
    );
    assert!(reloaded_task.gates.iter().any(|gate| {
        gate.kind == codex_agent_task_store::GateKind::Risk
            && gate.status == codex_agent_task_store::GateStatus::Passed
    }));
    assert!(reloaded_task.gates.iter().any(|gate| {
        gate.kind == codex_agent_task_store::GateKind::Review
            && gate.status == codex_agent_task_store::GateStatus::Pending
    }));

    std::fs::remove_file(risk_file).expect("high-risk test file should be removed");
    risk_file_cleanup.disarm();
}

#[tokio::test]
async fn multi_agent_v2_spawn_does_not_reuse_completed_generic_task_without_input_fingerprint() {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.ephemeral = false;
    let state_runtime = init_state_db(&config)
        .await
        .expect("typed spawn requires persistent test state");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_runtime),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = root.thread.codex.session.services.agent_control.clone();
    session.services.state_db = root.thread.codex.session.services.state_db.clone();
    session.thread_id = root.thread_id;
    let agent_control = session.services.agent_control.clone();
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let suffix = ThreadId::new().to_string().replace('-', "");
    let first_name = format!("explorer_first_{suffix}");
    let second_name = format!("explorer_duplicate_{suffix}");
    let objective = "trace the reusable explorer identity";
    let read_path = format!("src/reuse-{suffix}.rs");
    let assignment = |task_name: &str| {
        function_payload(json!({
            "task_name": task_name,
            "assignment": {
                "objective": objective,
                "acceptance_criteria": [{
                    "id": "criterion-1",
                    "text": "report the ownership trace"
                }],
                "read_scope": [{"path": read_path, "recursive": false}],
                "write_scope": [{"path": read_path, "recursive": false}],
                "required_evidence": ["bounded trace"],
                "stop_condition": "stop after reporting the trace"
            }
        }))
    };

    let first = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "spawn_agent",
            assignment(&first_name),
        ))
        .await
        .expect("first explorer should spawn");
    let (content, _) = expect_text_output(first);
    let first_result: serde_json::Value =
        serde_json::from_str(&content).expect("first spawn result should be json");
    let assignment_id = codex_agent_task_store::AssignmentId::parse(
        first_result["assignment_id"]
            .as_str()
            .expect("first spawn returns assignment id"),
    )
    .expect("assignment id parses");
    let binding = agent_control
        .task_coordinator()
        .binding_for_assignment(assignment_id)
        .expect("first explorer remains bound");
    let first_thread_id = ThreadId::from_string(
        binding
            .thread_id
            .as_deref()
            .expect("first explorer binding has a thread id"),
    )
    .expect("first explorer thread id parses");
    // Even an active producer has no observed-input fingerprint. Do not borrow
    // its result or let a duplicate spawn replace its occupied identity.
    let duplicate = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "spawn_agent",
            assignment(&first_name),
        ))
        .await
        .err()
        .expect("an unfingerprinted active explorer cannot be reused");
    assert!(matches!(duplicate, FunctionCallError::RespondToModel(ref message) if message.contains("already exists")));
    assert_eq!(
        agent_control.task_coordinator().binding_for_assignment(assignment_id).unwrap(),
        binding,
        "the original active assignment remains bound"
    );
    let active_followup = FollowupTaskHandlerV2
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "followup_task",
            function_payload(json!({"target": first_name, "message": "clarify the active task"})),
        ))
        .await
        .expect("an active typed attempt accepts followups");
    assert_eq!(expect_text_output(active_followup).1, Some(true));
    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == first_thread_id
            && matches!(op, Op::InterAgentCommunication { communication } if communication.trigger_turn)
    }));
    agent_control
        .task_coordinator()
        .store()
        .expect("typed task store remains available")
        .submit_agent_receipt(
            binding.attempt_id,
            codex_agent_task_store::ReceiptDraft {
                status: codex_agent_task_store::AgentStatusClaim::Completed,
                summary: "ownership trace completed".to_string(),
                criterion_results: vec![codex_agent_task_store::CriterionResult {
                    evidence_ref: None,
                    criterion_id: "criterion-1".to_string(),
                    status: codex_agent_task_store::CriterionStatus::Passed,
                    evidence: Some("bounded trace".to_string()),
                }],
                declared_changes: Vec::new(),
                validation_call_ids: Vec::new(),
                blockers: Vec::new(),
                risks: Vec::new(),
                next_action: None,
                architecture_contract: None,
            },
        )
        .await
        .expect("explorer result should seal");

    let ops_before_rejected_followups = manager.captured_ops().len();
    for _ in 0..2 {
        let rejected = FollowupTaskHandlerV2
            .handle(invocation(
                Arc::clone(&session),
                Arc::clone(&turn),
                "followup_task",
                function_payload(
                    json!({"target": first_name, "message": "repeat the sealed task"}),
                ),
            ))
            .await;
        let Err(FunctionCallError::RespondToModel(error)) = rejected else {
            panic!("sealed typed followup must fail before delivery");
        };
        assert!(error.contains("no active bound attempt"));
        assert!(error.contains("get_agent_task"));
        assert_eq!(manager.captured_ops().len(), ops_before_rejected_followups);
    }
    let cancelled = invocation(
        Arc::clone(&session),
        Arc::clone(&turn),
        "followup_task",
        function_payload(json!({"target": first_name, "message": "cancelled followup"})),
    );
    cancelled.cancellation_token.cancel();
    let Err(FunctionCallError::RespondToModel(error)) =
        FollowupTaskHandlerV2.handle(cancelled).await
    else {
        panic!("cancelled preflight must fail before delivery");
    };
    assert_eq!(error, "Follow-up task cancelled before delivery");
    assert_eq!(manager.captured_ops().len(), ops_before_rejected_followups);

    let queued = SendMessageHandlerV2
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "send_message",
            function_payload(json!({"target": first_name, "message": "information only"})),
        ))
        .await
        .expect("sealed typed agents still accept queue-only messages");
    assert_eq!(expect_text_output(queued).1, Some(true));
    let ops = manager.captured_ops();
    assert_eq!(ops.len(), ops_before_rejected_followups + 1);
    assert!(matches!(
        ops.last(),
        Some((id, Op::InterAgentCommunication { communication }))
            if *id == first_thread_id && !communication.trigger_turn
    ));
    let authorization = agent_control
        .task_coordinator()
        .get_agent_task_authorization(assignment_id)
        .await
        .expect("sealed task remains recoverable");
    assert_eq!(authorization.current_attempt.attempt_id, binding.attempt_id);
    assert_eq!(
        authorization.current_attempt.state,
        codex_agent_task_store::AttemptState::Completed
    );

    let fresh = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "spawn_agent",
            assignment(&second_name),
        ))
        .await
        .expect("completed evidence without an input fingerprint requires a fresh explorer");
    let (content, success) = expect_text_output(fresh);
    let fresh_result: serde_json::Value =
        serde_json::from_str(&content).expect("fresh spawn result should be json");
    assert_eq!(success, Some(true));
    assert_ne!(fresh_result["reused"], true);
    assert_ne!(fresh_result["assignment_id"], assignment_id.to_string());
    assert_ne!(fresh_result["attempt_id"], binding.attempt_id.to_string());
    let fresh_path = AgentPath::root()
        .join(&second_name)
        .expect("fresh candidate path is valid");
    let fresh_binding = agent_control
        .task_coordinator()
        .binding_for_agent_path(&fresh_path)
        .expect("fresh explorer must bind a new child");
    assert_eq!(
        fresh_result["assignment_id"],
        fresh_binding.assignment_id.to_string()
    );
    let fresh_thread_id = ThreadId::from_string(
        fresh_binding
            .thread_id
            .as_deref()
            .expect("fresh child thread id"),
    )
    .expect("fresh child thread id parses");
    assert_ne!(fresh_thread_id, first_thread_id);
    for thread_id in [first_thread_id, fresh_thread_id] {
        let _ = agent_control
            .shutdown_live_agent(thread_id)
            .await
            .expect("explorer child shuts down");
    }
}

#[tokio::test]
async fn multi_agent_v2_typed_spawn_admits_overlapping_write_claims() {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.ephemeral = false;
    let state_runtime = init_state_db(&config)
        .await
        .expect("typed spawn requires persistent test state");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_runtime),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = root.thread.codex.session.services.agent_control.clone();
    session.services.state_db = root.thread.codex.session.services.state_db.clone();
    session.thread_id = root.thread_id;
    let agent_control = session.services.agent_control.clone();
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let suffix = ThreadId::new().to_string().replace('-', "");
    let first_task_name = format!("typed_writer_one_{suffix}");
    let second_task_name = format!("typed_writer_two_{suffix}");
    let claim_path = format!("claimed-{suffix}");
    let overlapping_path = format!("{claim_path}/child");

    let first = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "spawn_agent",
            function_payload(json!({
                "task_name": first_task_name,
                "assignment": {
                    "objective": "edit the claimed path",
                    "acceptance_criteria": [{
                        "id": "criterion-1",
                        "text": "complete the bounded edit"
                    }],
                    "write_scope": [{"path": claim_path, "recursive": true}],
                    "required_evidence": ["focused validation passes"],
                    "stop_condition": "stop after the edit"
                }
            })),
        ))
        .await
        .expect("first typed writer should acquire its write claim");
    let (content, _) = expect_text_output(first);
    let first_result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let first_assignment_id = codex_agent_task_store::AssignmentId::parse(
        first_result["assignment_id"]
            .as_str()
            .expect("typed spawn should return assignment id"),
    )
    .expect("assignment id should parse");

    let second = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::clone(&session),
            Arc::clone(&turn),
            "spawn_agent",
            function_payload(json!({
                "task_name": second_task_name,
                "assignment": {
                    "objective": "edit an overlapping path",
                    "acceptance_criteria": [{
                        "id": "criterion-1",
                        "text": "complete the bounded edit"
                    }],
                    "write_scope": [{"path": overlapping_path, "recursive": true}],
                    "required_evidence": ["focused validation passes"],
                    "stop_condition": "stop after the edit"
                }
            })),
        ))
        .await
        .expect("overlapping typed writers should both be admitted");
    let (content, _) = expect_text_output(second);
    let second_result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let second_assignment_id = codex_agent_task_store::AssignmentId::parse(
        second_result["assignment_id"]
            .as_str()
            .expect("typed spawn should return assignment id"),
    )
    .expect("assignment id should parse");
    assert_eq!(second_result["integration_plan"], "root_owned");
    assert_eq!(
        agent_control
            .task_coordinator()
            .get_agent_task(second_assignment_id, Some(0))
            .await
            .expect("overlapping writer task remains readable")
            .assignment
            .integration_plan,
        codex_agent_task_store::IntegrationPlan::RootOwned
    );

    for assignment_id in [first_assignment_id, second_assignment_id] {
        let binding = agent_control
            .task_coordinator()
            .binding_for_assignment(assignment_id)
            .expect("each overlapping typed writer should remain bound");
        let child_thread_id = ThreadId::from_string(
            binding
                .thread_id
                .as_deref()
                .expect("typed binding should retain the child thread id"),
        )
        .expect("typed binding thread id should parse");
        let cleanup = agent_control
            .task_coordinator()
            .store()
            .expect("typed task store should remain available")
            .abandon_agent_task(
                codex_agent_task_store::TaskActor::Root,
                assignment_id,
                "test cleanup".to_string(),
            )
            .await;
        assert!(
            matches!(
                cleanup,
                Ok(_) | Err(codex_agent_task_store::StoreError::AttemptSealed(_))
            ),
            "unexpected cleanup failure: {cleanup:?}"
        );
        assert!(
            agent_control
                .task_coordinator()
                .remove_agent_task_binding(assignment_id)
                .await
                .expect("test cleanup should remove the sealed binding")
        );
        assert!(
            agent_control
                .task_coordinator()
                .binding_for_assignment(assignment_id)
                .is_none()
        );
        let _ = agent_control
            .shutdown_live_agent(child_thread_id)
            .await
            .expect("typed child shutdown should submit");
    }
}

#[tokio::test]
async fn multi_agent_v2_typed_spawn_failure_releases_write_claim() {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.ephemeral = false;
    let state_runtime = init_state_db(&config)
        .await
        .expect("typed spawn requires persistent test state");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_runtime),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = root.thread.codex.session.services.agent_control.clone();
    session.services.state_db = root.thread.codex.session.services.state_db.clone();
    session.thread_id = root.thread_id;
    let agent_control = session.services.agent_control.clone();
    let root_session_id = agent_control.session_id().to_string();
    set_turn_config(&mut turn, config);
    let task_name = format!(
        "failed_writer_{}",
        ThreadId::new().to_string().replace('-', "")
    );
    let failed_claim_path = format!("failed-claim-{task_name}");

    // Hold the spawn after admission and binding, then fail it there: a
    // reservation rejected before admission has nothing to roll back.
    let before_initial_submission = Arc::new(AgentControlTestBarrier::default());
    agent_control
        .set_before_initial_submission_barrier(Some(Arc::clone(&before_initial_submission)));
    let spawn_invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "task_name": task_name.clone(),
            "assignment": {
                "objective": "exercise spawn rollback",
                "acceptance_criteria": [{
                    "id": "criterion-1",
                    "text": "release the claim after failure"
                }],
                "write_scope": [{"path": failed_claim_path, "recursive": true}],
                "required_evidence": ["focused validation passes"],
                "stop_condition": "stop when spawning fails"
            }
        })),
    );
    let cancellation_token = spawn_invocation.cancellation_token.clone();
    let spawn_task = tokio::spawn(async move {
        SpawnAgentHandlerV2::default()
            .handle(spawn_invocation)
            .await
    });
    timeout(
        Duration::from_secs(15),
        before_initial_submission.wait_until_reached(),
    )
    .await
    .expect("typed spawn should be admitted and reach initial submission");
    agent_control.set_before_initial_submission_barrier(None);
    let agent_path = AgentPath::root()
        .join(&task_name)
        .expect("typed writer path should be valid");
    let assignment_id = agent_control
        .task_coordinator()
        .binding_for_agent_path(&agent_path)
        .expect("admitted assignment should be bound before the spawn fails")
        .assignment_id;

    cancellation_token.cancel();
    let error = timeout(Duration::from_secs(30), spawn_task)
        .await
        .expect("failed spawn should finish its rollback")
        .expect("typed spawn task should join")
        .err()
        .expect("a spawn cancelled before its initial submission must fail");
    assert_eq!(
        error,
        FunctionCallError::RespondToModel("collab spawn failed: turn aborted".to_string())
    );
    assert_eq!(
        agent_control
            .task_coordinator()
            .get_agent_task(assignment_id, Some(0))
            .await
            .expect("abandoned assignment should remain readable")
            .current_attempt
            .state,
        codex_agent_task_store::AttemptState::Abandoned
    );
    assert!(
        agent_control
            .task_coordinator()
            .binding_for_assignment(assignment_id)
            .is_none()
    );
    assert!(
        agent_control
            .task_coordinator()
            .store()
            .expect("typed task store should remain available")
            .list_agent_task_bindings(root_session_id, None)
            .await
            .expect("failed spawns should leave no durable bindings")
            .is_empty()
    );
}

#[tokio::test]
async fn multi_agent_v2_typed_spawn_is_root_only() {
    let (session, mut turn) = make_session_and_context().await;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: session.thread_id,
        depth: 1,
        agent_path: Some(AgentPath::try_from("/root/worker").expect("agent path")),
        agent_nickname: None,
        agent_role: None,
    });

    let err = match SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "task_name": "nested_typed_worker",
                "assignment": {
                    "objective": "modify the bounded path",
                    "acceptance_criteria": [{
                        "id": "criterion-1",
                        "text": "complete the bounded change"
                    }],
                    "write_scope": [{
                        "path": "src",
                        "recursive": true
                    }],
                    "stop_condition": "stop after validation"
                }
            })),
        ))
        .await
    {
        Ok(_) => panic!("non-root agents must not create durable typed assignments"),
        Err(err) => err,
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "spawn_agent: durable typed assignments are root-only".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_service_tier_override_validates_the_effective_child_model() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    {
        let (mut session, turn) = make_session_and_context().await;
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let output = SpawnAgentHandlerV2::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({"task_name": "test_agent",
                    "message": "inspect this repo",
                    "service_tier": ServiceTier::Fast.request_value()
                })),
            ))
            .await
            .expect("spawn_agent should accept a supported explicit service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(spawned_thread_id(&manager, &result.task_name).await)
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(
            snapshot.service_tier,
            Some(ServiceTier::Fast.request_value().to_string())
        );
    }

    {
        let (session, turn) = make_session_and_context().await;
        let err = SpawnAgentHandlerV2::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({"task_name": "test_agent",
                    "message": "inspect this repo",
                    "service_tier": "turbo"
                })),
            ))
            .await
            .err()
            .expect("unknown service tier should be rejected");

        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "Service tier `turbo` is not supported for model `gpt-6.1-sol`. Supported service tiers: priority"
                    .to_string()
            )
        );
    }

}

#[tokio::test]
async fn spawn_agent_service_tier_inheritance_preserves_supported_or_configured_tiers() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    {
        let (mut session, turn) = make_session_and_context().await;
        let mut turn = turn
            .with_model("gpt-5.5".to_string(), &session.services.models_manager)
            .await;
        let mut config = (*turn.config).clone();
        config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
        turn.config = Arc::new(config);
        set_session_service_tier(&session, ServiceTier::Fast.request_value()).await;
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let output = SpawnAgentHandlerV2::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({"task_name": "test_agent","message": "inspect this repo"})),
            ))
            .await
            .expect("spawn_agent should inherit a supported parent service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(spawned_thread_id(&manager, &result.task_name).await)
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(
            snapshot.service_tier,
            Some(ServiceTier::Fast.request_value().to_string())
        );
    }

    {
        let (mut session, turn) = make_session_and_context().await;
        let mut turn = turn
            .with_model("gpt-5.5".to_string(), &session.services.models_manager)
            .await;
        let mut config = (*turn.config).clone();
        config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
        turn.config = Arc::new(config);
        set_session_service_tier(&session, "turbo").await;
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let output = SpawnAgentHandlerV2::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({"task_name": "test_agent",
                    "message": "inspect this repo"
                })),
            ))
            .await
            .expect("spawn_agent should clear unsupported inherited service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(spawned_thread_id(&manager, &result.task_name).await)
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(snapshot.service_tier, None);
    }

    {
        let (mut session, mut turn) = make_session_and_context().await;
        tokio::fs::create_dir_all(&turn.config.codex_home)
            .await
            .expect("codex home should be created");
        let role_config_path = turn
            .config
            .codex_home
            .as_path()
            .join("service-tier-role.toml");
        tokio::fs::write(
            &role_config_path,
            r#"model = "gpt-5.5"
service_tier = "priority"
"#,
        )
        .await
        .expect("role config should be written");

        let role_name = "service-tier-role".to_string();
        let mut config = (*turn.config).clone();
        config.agent_roles.insert(
            role_name.clone(),
            AgentRoleConfig {
                description: Some("Role with a child service tier".to_string()),
                config_file: Some(role_config_path),
                nickname_candidates: None,
                config_snapshot: Default::default(),
            },
        );
        turn.config = Arc::new(config);
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let output = SpawnAgentHandlerV2::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(json!({"task_name": "test_agent",
                    "message": "inspect this repo"
                })),
            ))
            .await
            .expect("generic spawn ignores a configured role service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(spawned_thread_id(&manager, &result.task_name).await)
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(snapshot.service_tier, None);
    }
}

#[tokio::test]
async fn spawn_agent_role_service_tier_falls_back_to_supported_parent_tier() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    let (mut session, turn) = make_session_and_context().await;
    let mut turn = turn
        .with_model("gpt-5.5".to_string(), &session.services.models_manager)
        .await;
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home should be created");
    let role_config_path = turn.config.codex_home.as_path().join("tiered-role.toml");
    tokio::fs::write(
        &role_config_path,
        r#"model = "gpt-5.5"
service_tier = "turbo"
"#,
    )
    .await
    .expect("role config should be written");

    let role_name = "tiered-role".to_string();
    let mut config = (*turn.config).clone();
    config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
    config.agent_roles.insert(
        role_name.clone(),
        AgentRoleConfig {
            description: Some("Role with an unsupported child tier".to_string()),
            config_file: Some(role_config_path),
            nickname_candidates: None,
            config_snapshot: Default::default(),
        },
    );
    turn.config = Arc::new(config);
    set_session_service_tier(&session, ServiceTier::Fast.request_value()).await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({"task_name": "test_agent",
                "message": "inspect this repo"
            })),
        ))
        .await
        .expect("spawn_agent should fall back to the supported parent tier");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let snapshot = manager
        .get_thread(spawned_thread_id(&manager, &result.task_name).await)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(
        snapshot.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
}

#[tokio::test]
async fn spawn_agent_uses_the_service_tier_selected_after_the_turn_started() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    // Turning fast mode off stores the explicit `default` choice; an explicit
    // request for it is valid for every model.
    for (requested, turned_off_mid_turn) in [(None, true), (Some("default"), false)] {
        let (mut session, turn) = make_session_and_context().await;
        let mut turn = turn
            .with_model("gpt-5.5".to_string(), &session.services.models_manager)
            .await;
        let mut config = (*turn.config).clone();
        config.service_tier = Some(ServiceTier::Fast.request_value().to_string());
        turn.config = Arc::new(config);
        set_session_service_tier(&session, ServiceTier::Fast.request_value()).await;
        if turned_off_mid_turn {
            session
                .update_settings(crate::session::SessionSettingsUpdate {
                    service_tier: Some(None),
                    ..Default::default()
                })
                .await
                .expect("service tier update");
        }
        let manager = thread_manager();
        let root = manager
            .start_thread((*turn.config).clone())
            .await
            .expect("root thread should start");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;

        let mut args = json!({"task_name": "test_agent","message": "inspect this repo"});
        if let Some(requested) = requested {
            args["service_tier"] = json!(requested);
        }
        let output = SpawnAgentHandlerV2::default()
            .handle(invocation(
                Arc::new(session),
                Arc::new(turn),
                "spawn_agent",
                function_payload(args),
            ))
            .await
            .expect("spawn_agent should accept the default service tier");
        let (content, _) = expect_text_output(output);
        let result: SpawnAgentResult =
            serde_json::from_str(&content).expect("spawn_agent result should be json");
        let snapshot = manager
            .get_thread(spawned_thread_id(&manager, &result.task_name).await)
            .await
            .expect("spawned agent thread should exist")
            .config_snapshot()
            .await;

        assert_eq!(
            snapshot.service_tier.as_deref(),
            Some(codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE),
            "requested={requested:?}, turned_off_mid_turn={turned_off_mid_turn}"
        );
    }
}

#[tokio::test]
async fn spawn_agent_role_service_tier_does_not_hide_invalid_spawn_request() {
    let (session, mut turn) = make_session_and_context().await;
    tokio::fs::create_dir_all(&turn.config.codex_home)
        .await
        .expect("codex home should be created");
    let role_config_path = turn.config.codex_home.as_path().join("tiered-role.toml");
    tokio::fs::write(
        &role_config_path,
        r#"model = "gpt-5.4"
service_tier = "priority"
"#,
    )
    .await
    .expect("role config should be written");

    let role_name = "tiered-role".to_string();
    let mut config = (*turn.config).clone();
    config.agent_roles.insert(
        role_name.clone(),
        AgentRoleConfig {
            description: Some("Role with a supported child tier".to_string()),
            config_file: Some(role_config_path),
            nickname_candidates: None,
            config_snapshot: Default::default(),
        },
    );
    turn.config = Arc::new(config);

    let result = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({"task_name": "test_agent",
                "message": "inspect this repo",
                "service_tier": "turbo"
            })),
        ))
        .await;

    assert_eq!(
        result.err(),
        Some(FunctionCallError::RespondToModel(
            "Service tier `turbo` is not supported for model `gpt-6.1-sol`. Supported service tiers: priority"
                .to_string()
        ))
    );
}



#[tokio::test]
async fn multi_agent_v2_full_history_fork_accepts_explicit_service_tier() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
    }

    let (mut session, turn) = make_session_and_context().await;
    let mut turn = turn
        .with_model("gpt-5.4".to_string(), &session.services.models_manager)
        .await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.allow_full_history_forks = true;
    set_turn_config(&mut turn, config);
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "fork_with_tier",
                "fork_turns": "all",
                "service_tier": ServiceTier::Fast.request_value()
            })),
        ))
        .await
        .expect("multi-agent v2 full-history fork should accept explicit service tier");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let child_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(
            session.thread_id,
            &turn.session_source,
            result.task_name.as_str(),
        )
        .await
        .expect("spawned task name should resolve");
    let snapshot = manager
        .get_thread(child_thread_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(
        snapshot.service_tier,
        Some(ServiceTier::Fast.request_value().to_string())
    );
    assert_eq!(snapshot.model, DEFAULT_SPAWN_AGENT_MODEL);
    assert_eq!(
        snapshot.reasoning_effort,
        Some(DEFAULT_SPAWN_AGENT_REASONING_EFFORT)
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_partial_fork_keeps_uniform_defaults() {
    let (mut session, mut turn) = make_session_and_context().await;
    let _role_name = install_role_with_model_override(&mut turn).await;
    let expected_provider_id = turn.config.model_provider_id.clone();
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    let turn = TurnContext {
        config: Arc::new(config),
        multi_agent_version: codex_protocol::protocol::MultiAgentVersion::V2,
        ..turn
    };

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "partial_fork",
                "fork_turns": "1"
            })),
        ))
        .await
        .expect("partial fork should allow agent_type overrides");
    let (content, _) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert_eq!(result["task_name"], "/root/partial_fork");
    let agent_id = manager
        .captured_ops()
        .into_iter()
        .map(|(thread_id, _)| thread_id)
        .find(|thread_id| *thread_id != root.thread_id)
        .expect("spawned agent should receive an op");
    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;

    assert_eq!(snapshot.model, DEFAULT_SPAWN_AGENT_MODEL);
    assert_eq!(snapshot.model_provider_id, expected_provider_id);
    assert_eq!(snapshot.reasoning_effort, Some(DEFAULT_SPAWN_AGENT_REASONING_EFFORT));
}



#[tokio::test]
async fn multi_agent_v2_spawn_requires_task_name() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo"
        })),
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("missing task_name should be rejected");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("missing task_name should surface as a model-facing error");
    };
    assert!(message.contains("missing field `task_name`"));
}

#[tokio::test]
async fn multi_agent_v2_spawn_rejects_legacy_items_field() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo",
            "items": [{"type": "text", "text": "inspect this repo"}],
            "task_name": "worker"
        })),
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("legacy items field should be rejected");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("legacy items field should surface as a model-facing error");
    };
    assert!(message.contains("unknown field `items`"));
}

#[tokio::test]
async fn spawn_agent_errors_when_manager_dropped() {
    let (session, turn) = make_session_and_context().await;
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"task_name": "test_agent","message": "hello"})),
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("spawn should fail without a manager");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel("collab manager unavailable".to_string())
    );
}

#[test_case::test_case(false; "direct_encrypted")]
#[test_case::test_case(true; "code_mode_plaintext")]
#[tokio::test]
async fn multi_agent_v2_registered_message_batch_preserves_delivery_and_partial_failure(
    code_mode: bool,
) {
    async fn dispatch(
        mut invocation: ToolInvocation,
        code_mode: bool,
    ) -> codex_protocol::models::FunctionCallOutputPayload {
        invocation.call_id = format!("message-{}", ThreadId::new());
        if code_mode {
            invocation.source = crate::tools::context::ToolCallSource::CodeMode {
                cell_id: "message-cell".to_string(),
                parent_call_id: Some("message-exec".to_string()),
                runtime_tool_call_id: invocation.call_id.clone(),
                nested_deadline: None,
                cancellation_cause: None,
            };
        }
        let router = Arc::new(crate::tools::router::ToolRouter::from_context(
            invocation.step_context.as_ref(),
            crate::tools::router::ToolRouterParams {
                tool_suggest_candidates: None,
                deferred_mcp_tools: None,
                mcp_tools: None,
                extension_tool_executors: Vec::new(),
                dynamic_tools: &[],
                exposure_identity: Default::default(),
            },
            &Default::default(),
        ));
        let requested_tool_name = invocation.tool_name.name.clone();
        invocation.tool_name = router
            .registered_tool_names_for_test()
            .into_iter()
            .find(|name| name.name == requested_tool_name)
            .expect("registered message tool");
        assert!(invocation.step_context.set_tool_router(router).is_ok());
        let runtime = crate::tools::parallel::ToolCallRuntime::new(
            invocation.session,
            invocation.step_context,
            invocation.tracker,
        );
        let call = crate::tools::router::ToolCall {
            tool_name: invocation.tool_name,
            call_id: invocation.call_id,
            payload: invocation.payload,
        };
        let response = match runtime
            .handle_tool_call_with_source(
                call.clone(),
                invocation.source,
                invocation.cancellation_token,
            )
            .await
        {
            Ok(result) => result.into_response(),
            Err(FunctionCallError::RespondToModel(message)) => {
                crate::tools::parallel::ToolCallRuntime::failure_response_for_message(
                    &call, message,
                )
            }
            Err(error) => panic!("registered message failed: {error}"),
        };
        let ResponseInputItem::FunctionCallOutput { output, .. } = response else {
            panic!("message must return a function result");
        };
        output
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("enable v2");
    // Keep the root and all three recipients resident while exercising message delivery.
    config.multi_agent_v2.max_concurrent_threads_per_session = 4;
    set_turn_config(&mut turn, config);
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root");
    let foreign = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("other session");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    for name in ["batch_one", "batch_two", "unselected"] {
        SpawnAgentHandlerV2::default()
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "spawn_agent",
                function_payload(json!({"task_name": name, "message": "initial task"})),
            ))
            .await
            .expect("child starts");
    }
    let before = manager.captured_ops().len();
    let output = dispatch(
        invocation(
            session.clone(),
            turn.clone(),
            "send_message",
            function_payload(json!({
                "targets": ["batch_one", foreign.thread_id.to_string(), "missing", "/root/batch_two"],
                "message": "shared information"
            })),
        ),
        code_mode,
    )
    .await;
    assert_eq!(
        output.success,
        Some(false),
        "partial failure is not success"
    );
    let body: serde_json::Value = serde_json::from_str(&output.body.to_text().unwrap()).unwrap();
    let recipients = body["recipients"]
        .as_array()
        .expect("per-recipient outcomes");
    assert_eq!(recipients.len(), 4);
    assert_eq!(
        recipients[0],
        json!({"target": "batch_one", "status": "queued"})
    );
    assert_eq!(recipients[1]["status"], "failed");
    assert!(recipients[1]["error"].is_string());
    assert_eq!(recipients[2]["status"], "failed");
    assert_eq!(
        recipients[3],
        json!({"target": "/root/batch_two", "status": "queued"})
    );
    let mut deliveries: Vec<_> = manager.captured_ops()[before..]
        .iter()
        .filter_map(|(_, op)| {
            if let Op::InterAgentCommunication { communication } = op {
                assert!(!communication.trigger_turn);
                assert_message_encoding(communication, "shared information", code_mode);
                assert!(communication.other_recipients.is_empty());
                Some(communication.recipient.to_string())
            } else {
                None
            }
        })
        .collect();
    deliveries.sort();
    assert_eq!(deliveries, vec!["/root/batch_one", "/root/batch_two"]);

    let before = manager.captured_ops().len();
    let cancelled = invocation(
        session.clone(),
        turn.clone(),
        "send_message",
        function_payload(json!({"targets": ["batch_one", "batch_two"], "message": "cancelled"})),
    );
    cancelled.cancellation_token.cancel();
    let output = dispatch(cancelled, code_mode).await;
    assert_eq!(output.success, None);
    assert!(
        serde_json::to_string(&output)
            .expect("cancelled output")
            .contains("aborted by user")
    );
    assert_eq!(
        manager.captured_ops().len(),
        before,
        "cancelled batch must not enqueue updates"
    );

    for args in [
        json!({"target": "batch_one", "targets": ["batch_two"], "message": "invalid"}),
        json!({"targets": [], "message": "invalid"}),
        json!({"targets": ["batch_one"], "message": " "}),
    ] {
        let before = manager.captured_ops().len();
        let output = dispatch(
            invocation(
                session.clone(),
                turn.clone(),
                "send_message",
                function_payload(args),
            ),
            code_mode,
        )
        .await;
        assert_eq!(output.success, Some(false));
        assert_eq!(
            manager.captured_ops().len(),
            before,
            "invalid input must not deliver anything"
        );
    }
    for (tool, trigger_turn) in [("send_message", false), ("followup_task", true)] {
        let before = manager.captured_ops().len();
        let output = dispatch(
            invocation(
                session.clone(),
                turn.clone(),
                tool,
                function_payload(json!({"target": "batch_one", "message": "single update"})),
            ),
            code_mode,
        )
        .await;
        assert_eq!(output.success, Some(true));
        let ops = manager.captured_ops();
        let deliveries: Vec<_> = ops[before..]
            .iter()
            .filter_map(|(_, op)| match op {
                Op::InterAgentCommunication { communication } => Some(communication),
                _ => None,
            })
            .collect();
        assert_eq!(deliveries.len(), 1);
        let communication = deliveries[0];
        assert_eq!(communication.trigger_turn, trigger_turn);
        assert_eq!(communication.recipient.as_str(), "/root/batch_one");
        assert_message_encoding(communication, "single update", code_mode);
    }

    fn assert_message_encoding(
        communication: &InterAgentCommunication,
        message: &str,
        code_mode: bool,
    ) {
        let item = serde_json::to_value(communication.to_model_input_item()).unwrap();
        if code_mode {
            assert_eq!(communication.content, message);
            assert_eq!(communication.encrypted_content, None);
            assert_eq!(
                item["content"],
                json!([{"type": "input_text", "text": message}])
            );
        } else {
            assert!(communication.content.is_empty());
            assert_eq!(communication.encrypted_content.as_deref(), Some(message));
            assert_eq!(
                item["content"][1],
                json!({
                    "type": "encrypted_content",
                    "encrypted_content": message,
                })
            );
        }
    }
}

#[tokio::test]
async fn multi_agent_v2_spawn_returns_path_and_send_message_accepts_relative_path() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "encrypted-spawn-message",
                "task_name": "test_process"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let (content, success) = expect_text_output(spawn_output);
    let shape: serde_json::Value = serde_json::from_str(&content).expect("spawn JSON");
    assert!(shape.get("agent_id").is_none());
    assert!(shape.get("nickname").is_none());
    assert_eq!(success, Some(true));
    let spawn_result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn result should parse");
    assert_eq!(spawn_result.task_name, "/root/test_process");
    assert_eq!(spawn_result.nickname, None);

    let child_thread_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "test_process")
        .await
        .expect("relative path should resolve");
    let child_snapshot = manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(
        child_snapshot.session_source.get_agent_path().as_deref(),
        Some("/root/test_process")
    );
    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == child_thread_id
            && matches!(
                op,
                Op::UserInput { items, .. }
                    if items.iter().any(|item| matches!(
                        item,
                        UserInput::Text { text, .. }
                            if text.starts_with("<task_capsule_v1>")
                                && text.ends_with("</task_capsule_v1>")
                    ))
            )
    }));

    SendMessageHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "send_message",
            function_payload(json!({
                "target": "test_process",
                "message": "encrypted-send-message"
            })),
        ))
        .await
        .expect("send_message should accept v2 path");

    assert!(manager.captured_ops().iter().any(|(id, op)| {
        *id == child_thread_id
            && matches!(
                op,
                Op::InterAgentCommunication { communication }
                    if communication.author == AgentPath::root()
                        && communication.recipient.as_str() == "/root/test_process"
                        && communication.other_recipients.is_empty()
                        && communication.content.is_empty()
                        && communication.encrypted_content.as_deref() == Some("encrypted-send-message")
                        && !communication.trigger_turn
            )
    }));
}

#[tokio::test]
async fn multi_agent_v2_spawn_rejects_legacy_fork_context() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let err = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker",
                "fork_context": true
            })),
        ))
        .await
        .err()
        .expect("legacy fork_context should be rejected");

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "fork_context is not supported in MultiAgentV2; use fork_turns instead".to_string()
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_spawn_rejects_invalid_fork_turns_string() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let before = manager.captured_ops().len();
    for fork_turns in ["banana", "0"] {
        let err = SpawnAgentHandlerV2::default()
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "spawn_agent",
                function_payload(json!({
                    "message": "inspect this repo",
                    "task_name": "worker",
                    "fork_turns": fork_turns
                })),
            ))
            .await
            .err()
            .expect("invalid fork_turns should be rejected");

        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "fork_turns must be `none` or an integer from 1 through 5".to_string()
            )
        );
    }
    assert_eq!(manager.captured_ops().len(), before);
}

#[test_case::test_case(false; "direct_encrypted")]
#[test_case::test_case(true; "code_mode_plaintext")]
#[tokio::test]
async fn multi_agent_v2_send_message_rejects_progress_to_root_from_child(code_mode: bool) {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let child_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_thread_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "inspect this repo".to_string(),
                text_elements: Vec::new(),
            }],
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(child_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker spawn should succeed")
        .thread_id;
    session.thread_id = child_thread_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path.clone()),
        agent_nickname: None,
        agent_role: None,
    });

    let mut call = invocation(
        Arc::new(session),
        Arc::new(turn),
        "send_message",
        function_payload(json!({"target": "/root", "message": "progress-update"})),
    );
    if code_mode {
        call.source = crate::tools::context::ToolCallSource::CodeMode {
            cell_id: "progress-cell".into(),
            parent_call_id: Some("progress-exec".into()),
            runtime_tool_call_id: call.call_id.clone(),
            nested_deadline: None,
            cancellation_cause: None,
        };
    }
    match SendMessageHandlerV2.handle(call).await {
        Err(FunctionCallError::RespondToModel(message)) => {
            assert!(message.contains("report to ancestors only at the end"), "{message}");
        }
        _ => panic!("child progress must be rejected for both call surfaces"),
    }
    assert!(!manager.captured_ops().iter().any(|(id, op)| {
        *id == root.thread_id && matches!(op,
            Op::InterAgentCommunication { communication }
                if communication.author == child_path
                    && (communication.content == "progress-update"
                        || communication.encrypted_content.as_deref() == Some("progress-update")))
    }));

}

#[tokio::test]
async fn multi_agent_v2_followup_task_rejects_root_target_from_child() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let child_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_thread_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "inspect this repo".to_string(),
                text_elements: Vec::new(),
            }],
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(child_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker spawn should succeed")
        .thread_id;
    session.thread_id = child_thread_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path),
        agent_nickname: None,
        agent_role: None,
    });

    let Err(err) = FollowupTaskHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "followup_task",
            function_payload(json!({
                "target": "/root",
                "message": "run this",
            })),
        ))
        .await
    else {
        panic!("followup_task should reject the root target");
    };

    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "Follow-up tasks can't target the root agent".to_string()
        )
    );
    let root_ops = manager
        .captured_ops()
        .into_iter()
        .filter_map(|(id, op)| (id == root.thread_id).then_some(op))
        .collect::<Vec<_>>();
    assert!(!root_ops.iter().any(|op| matches!(op, Op::Interrupt)));
    assert!(
        !root_ops
            .iter()
            .any(|op| matches!(op, Op::InterAgentCommunication { .. }))
    );
}

#[tokio::test]
async fn multi_agent_v2_list_agents_returns_completed_status_without_encrypted_spawn_preview() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    let child_thread = manager
        .get_thread(agent_id)
        .await
        .expect("child thread should exist");
    let child_turn = child_thread.codex.session.new_default_turn().await;
    let binding = session
        .services
        .agent_control
        .task_coordinator()
        .binding_for_source(&child_turn.session_source)
        .expect("spawned child has an exact task binding");
    child_turn.agent_task_binding.set(Some(binding)).unwrap();
    child_thread
        .codex
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                turn_id: child_turn.sub_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        )
        .await;
    child_thread
        .codex
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                surfaced_result: None,
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some("done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                timing: None,
            }),
        )
        .await;

    let coordinator = session.services.agent_control.task_coordinator();
    let binding = coordinator
        .binding_for_agent_path(&AgentPath::root().join("worker").unwrap())
        .expect("ordinary task retains durable identity");
    let task = coordinator
        .get_agent_task(binding.assignment_id, Some(0))
        .await
        .unwrap();
    assert_eq!(
        task.current_attempt.state,
        codex_agent_task_store::AttemptState::Completed
    );
    let receipt = task
        .receipt
        .expect("host persists plain-message completion");
    assert_eq!(receipt.summary, "done");
    assert!(receipt.validation_call_ids.is_empty());
    assert!(receipt.criterion_results.iter().all(|result| result.status
        == codex_agent_task_store::CriterionStatus::NotRun
        && result.evidence_ref.is_none()));

    let output = ListAgentsHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, success) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    let agent_names = result
        .agents
        .iter()
        .map(|agent| agent.agent_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(agent_names, vec!["/root", "/root/worker"]);
    let root_agent = result
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root")
        .expect("root agent should be listed");
    assert_eq!(root_agent.last_task_message.as_deref(), Some("Main thread"));
    let worker = result
        .agents
        .iter()
        .find(|agent| agent.agent_name == "/root/worker")
        .expect("worker agent should be listed");
    assert_eq!(
        worker.agent_status,
        json!({
            "completed": "done"
        })
    );
    assert_eq!(worker.last_task_message.as_deref(), Some("TaskCapsuleV1"));
    assert_eq!(success, Some(true));

    let mut previous_attempt = binding.attempt_id;
    for ordinal in 1..=2 {
        let followup = FollowupTaskHandlerV2
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "followup_task",
                function_payload(
                    json!({"target": "worker", "message": format!("followup {ordinal}")}),
                ),
            ))
            .await
            .expect("completed plain child accepts followup");
        assert_eq!(expect_text_output(followup).1, Some(true));
        assert_eq!(
            coordinator
                .binding_for_agent_path(&AgentPath::root().join("worker").unwrap())
                .unwrap()
                .attempt_id,
            previous_attempt,
            "queued followup delivery does not retire an attempt before the next turn"
        );
        assert!(manager.captured_ops().iter().any(|(id, op)| *id == agent_id && matches!(op, Op::InterAgentCommunication { communication } if communication.trigger_turn && communication.recipient.as_str() == "/root/worker")));
        let next_turn = child_thread.codex.session.new_default_turn().await;
        // The manager records operations without running model turns. Exercise the same
        // preparation boundary run_turn uses before any tool or model work.
        coordinator
            .prepare_legacy_agent_turn(&next_turn.session_source, agent_id)
            .await
            .unwrap();
        next_turn
            .agent_task_binding
            .set(Some(
                coordinator
                    .binding_for_source(&next_turn.session_source)
                    .expect("followup turn has its renewed task binding"),
            ))
            .unwrap();
        let active = coordinator
            .get_agent_task(binding.assignment_id, Some(0))
            .await
            .unwrap();
        assert_eq!(active.current_attempt.ordinal, ordinal);
        assert_ne!(active.current_attempt.attempt_id, previous_attempt);
        assert!(active.receipt.is_none());
        previous_attempt = active.current_attempt.attempt_id;
        let summary = format!("followup result {ordinal}");
        child_thread
            .codex
            .session
            .send_event(
                next_turn.as_ref(),
                EventMsg::TurnStarted(codex_protocol::protocol::TurnStartedEvent {
                    turn_id: next_turn.sub_id.clone(),
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: Default::default(),
                }),
            )
            .await;
        child_thread
            .codex
            .session
            .send_event(
                next_turn.as_ref(),
                EventMsg::TurnComplete(TurnCompleteEvent {
                    surfaced_result: None,
                    turn_id: next_turn.sub_id.clone(),
                    last_agent_message: Some(summary.clone()),
                    error: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                    timing: None,
                }),
            )
            .await;
        let latest = coordinator
            .get_agent_task(binding.assignment_id, Some(0))
            .await
            .unwrap();
        assert_eq!(latest.receipt.unwrap().summary, summary);
        let listed = ListAgentsHandlerV2
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "list_agents",
                function_payload(json!({})),
            ))
            .await
            .unwrap();
        let listed: ListAgentsResult = serde_json::from_str(&expect_text_output(listed).0).unwrap();
        assert_eq!(
            listed
                .agents
                .iter()
                .find(|agent| agent.agent_name == "/root/worker")
                .unwrap()
                .agent_status,
            json!({"completed": summary})
        );
    }
}

#[tokio::test]
async fn multi_agent_v2_list_agents_filters_by_relative_path_prefix() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config.clone());
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let researcher_path = AgentPath::from_string("/root/researcher".to_string()).expect("path");
    let worker_path = AgentPath::from_string("/root/researcher/worker".to_string()).expect("path");
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config.clone(),
            vec![UserInput::Text {
                text: "research".to_string(),
                text_elements: Vec::new(),
            }],
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(researcher_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("researcher agent should spawn");
    session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            config,
            vec![UserInput::Text {
                text: "build".to_string(),
                text_elements: Vec::new(),
            }],
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 2,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker agent should spawn");

    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(researcher_path),
        agent_nickname: None,
        agent_role: None,
    });

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    for prefix in ["worker", "worker/", "/root/researcher/worker/"] {
        let output = ListAgentsHandlerV2
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "list_agents",
                function_payload(json!({
                    "path_prefix": prefix
                })),
            ))
            .await
            .expect("list_agents should succeed");
        let (content, _) = expect_text_output(output);
        let result: ListAgentsResult =
            serde_json::from_str(&content).expect("list_agents result should be json");

        assert_eq!(result.agents.len(), 1);
        assert_eq!(result.agents[0].agent_name, worker_path.as_str());
        assert_eq!(result.agents[0].last_task_message.as_deref(), Some("build"));
    }
}

#[tokio::test]
async fn multi_agent_v2_list_agents_omits_closed_agents() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    session
        .services
        .agent_control
        .close_agent(agent_id)
        .await
        .expect("close_agent should succeed");

    let output = ListAgentsHandlerV2
        .handle(invocation(
            session,
            turn,
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    assert_eq!(result.agents.len(), 1);
    assert_eq!(result.agents[0].agent_name, "/root");
    assert_eq!(
        result.agents[0].last_task_message.as_deref(),
        Some("Main thread")
    );
}

#[tokio::test]
async fn multi_agent_v2_list_agents_keeps_interrupted_resident_agents() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    let agent_path = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .expect("worker metadata should exist")
        .agent_path
        .expect("worker path should exist");
    let interrupt_output = InterruptAgentHandler
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "interrupt_agent",
            function_payload(json!({"target": "worker"})),
        ))
        .await
        .expect("interrupt_agent should succeed");
    let _ = expect_text_output(interrupt_output);

    let output = ListAgentsHandlerV2
        .handle(invocation(
            session,
            turn,
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");

    assert_eq!(result.agents.len(), 2);
    assert_eq!(result.agents[0].agent_name, "/root");
    assert_eq!(
        result.agents[0].last_task_message.as_deref(),
        Some("Main thread")
    );
    assert_eq!(result.agents[1].agent_name, agent_path.as_str());
}

#[tokio::test]
async fn multi_agent_v2_send_message_queues_task_name_alias_to_spawned_agent() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "task_name": "worker",
                "message": "boot worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let (content, success) = expect_text_output(output);
    assert_eq!(success, Some(true));
    let spawned: serde_json::Value = serde_json::from_str(&content).expect("spawn JSON");
    assert_eq!(spawned["task_name"], "/root/worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");

    let output = SendMessageHandlerV2
        .handle(invocation(
            session,
            turn,
            "send_message",
            function_payload(json!({
                "task_name": spawned["task_name"],
                "message": "encrypted-alias-message"
            })),
        ))
        .await
        .expect("send_message should accept spawn_agent's task_name without target");
    let (_, success) = expect_text_output(output);
    assert_eq!(success, Some(true));

    let ops = manager.captured_ops();
    let messages: Vec<_> = ops
        .iter()
        .filter_map(|(id, op)| match op {
            Op::InterAgentCommunication { communication }
                if communication.encrypted_content.as_deref()
                    == Some("encrypted-alias-message") =>
            {
                Some((id, communication))
            }
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 1, "queue the message exactly once");
    let (recipient_id, communication) = messages[0];
    assert_eq!(*recipient_id, agent_id);
    assert_eq!(communication.author, AgentPath::root());
    assert_eq!(communication.recipient.as_str(), "/root/worker");
    assert!(communication.other_recipients.is_empty());
    assert!(communication.content.is_empty());
    assert!(
        !communication.trigger_turn,
        "send_message must remain queue-only"
    );
}

#[tokio::test]
async fn multi_agent_v2_send_message_rejects_disagreeing_target_and_task_name() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    session.services.agent_control = manager.agent_control();

    let Err(err) = SendMessageHandlerV2
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "send_message",
            function_payload(json!({
                "target": "a",
                "task_name": "b",
                "message": "do not deliver"
            })),
        ))
        .await
    else {
        panic!("disagreeing recipient names must be rejected");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("recipient disagreement should surface as a model-facing error");
    };
    assert_eq!(
        message,
        "`target` (a) and `task_name` (b) name different recipients; supply one."
    );
    assert!(
        !manager
            .captured_ops()
            .iter()
            .any(|(_, op)| matches!(op, Op::InterAgentCommunication { .. } | Op::Interrupt))
    );
}

#[tokio::test]
async fn multi_agent_v2_send_message_rejects_legacy_items_field() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let invocation = invocation(
        session,
        turn,
        "send_message",
        function_payload(json!({
            "target": agent_id.to_string(),
            "items": [
                {"type": "mention", "name": "drive", "path": "app://google_drive"},
                {"type": "text", "text": "read the folder"}
            ]
        })),
    );

    let Err(err) = SendMessageHandlerV2.handle(invocation).await else {
        panic!("legacy items field should be rejected in v2");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("legacy items field should surface as a model-facing error");
    };
    assert!(message.contains("unknown field `items`"));
}

#[tokio::test]
async fn multi_agent_v2_send_message_rejects_interrupt_parameter() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");

    let invocation = invocation(
        session,
        turn,
        "send_message",
        function_payload(json!({
            "target": agent_id.to_string(),
            "message": "continue",
            "interrupt": true
        })),
    );

    let Err(err) = SendMessageHandlerV2.handle(invocation).await else {
        panic!("send_message interrupt parameter should be rejected");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("expected model-facing parse error");
    };
    assert!(message.starts_with(
        "failed to parse function arguments: unknown field `interrupt`, expected one of `target`, `task_name`, `targets`, `message`"
    ));

    let ops = manager.captured_ops();
    let ops_for_agent: Vec<&Op> = ops
        .iter()
        .filter_map(|(id, op)| (*id == agent_id).then_some(op))
        .collect();
    assert!(!ops_for_agent.iter().any(|op| matches!(op, Op::Interrupt)));
    assert!(!ops_for_agent.iter().any(|op| matches!(
        op,
        Op::InterAgentCommunication { communication }
            if communication.author == AgentPath::root()
                && communication.recipient.as_str() == "/root/worker"
                && communication.other_recipients.is_empty()
                && communication.content.is_empty()
                && communication.encrypted_content.as_deref() == Some("continue")
                && !communication.trigger_turn
    )));
}

#[tokio::test]
async fn multi_agent_v2_followup_task_rejects_legacy_items_field() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let invocation = invocation(
        session,
        turn,
        "followup_task",
        function_payload(json!({
            "target": agent_id.to_string(),
            "items": [{"type": "text", "text": "continue"}],
        })),
    );

    let Err(err) = FollowupTaskHandlerV2.handle(invocation).await else {
        panic!("legacy items field should be rejected in v2");
    };
    let FunctionCallError::RespondToModel(message) = err else {
        panic!("legacy items field should surface as a model-facing error");
    };
    assert!(message.contains("unknown field `items`"));
}

#[tokio::test]
async fn multi_agent_v2_interrupted_turn_does_not_notify_parent() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = turn.config.as_ref().clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let thread = manager
        .get_thread(agent_id)
        .await
        .expect("worker thread should exist");

    let aborted_turn = thread.codex.session.new_default_turn().await;
    thread
        .codex
        .session
        .send_event(
            aborted_turn.as_ref(),
            EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some(aborted_turn.sub_id.clone()),
                reason: TurnAbortReason::Interrupted,
                completed_at: None,
                duration_ms: None,
                timing: None,
            }),
        )
        .await;

    let notifications = manager
        .captured_ops()
        .into_iter()
        .filter_map(|(id, op)| {
            (id == root.thread_id)
                .then_some(op)
                .and_then(|op| match op {
                    Op::InterAgentCommunication { communication }
                        if communication.author.as_str() == "/root/worker"
                            && communication.recipient == AgentPath::root()
                            && communication.other_recipients.is_empty()
                            && !communication.trigger_turn =>
                    {
                        Some(communication.content)
                    }
                    _ => None,
                })
        })
        .collect::<Vec<_>>();

    assert_eq!(notifications, Vec::<String>::new());
}

#[tokio::test]
async fn multi_agent_v2_spawn_surfaces_task_name_validation_errors() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "inspect this repo",
            "task_name": "BadName"
        })),
    );
    let Err(err) = SpawnAgentHandlerV2::default().handle(invocation).await else {
        panic!("invalid agent name should be rejected");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "agent_name must use only lowercase letters, digits, and underscores".to_string()
        )
    );
}

#[tokio::test]
async fn spawn_agent_reapplies_runtime_sandbox_when_role_does_not_override_permissions() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager.start_thread((*turn.config).clone()).await.expect("start root");
    session.thread_id = root.thread_id;
    session.services.agent_control = manager.agent_control();
    let expected_sandbox = turn.config.legacy_sandbox_policy();
    let mut expected_file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(&expected_sandbox, turn.cwd());
    expected_file_system_sandbox_policy
        .entries
        .push(FileSystemSandboxEntry {
            path: FileSystemPath::GlobPattern {
                pattern: "**/.env".to_string(),
            },
            access: FileSystemAccessMode::Deny,
        });
    let expected_network_sandbox_policy = NetworkSandboxPolicy::from(&expected_sandbox);
    let expected_permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
        SandboxEnforcement::from_legacy_sandbox_policy(&expected_sandbox),
        &expected_file_system_sandbox_policy,
        expected_network_sandbox_policy,
    );
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy should be set");
    let config = (*turn.config).clone();
    set_turn_config(&mut turn, config);
    turn.permission_profile = expected_permission_profile.clone();
    assert_ne!(
        expected_permission_profile,
        turn.config.permissions.effective_permission_profile(),
        "test requires a runtime profile override that differs from base config"
    );

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({"task_name": "test_agent",
            "message": "await this command"
        })),
    );
    let output = SpawnAgentHandlerV2::default()
        .handle(invocation)
        .await
        .expect("spawn_agent should succeed");
    let (content, _) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let agent_id = spawned_thread_id(&manager, &result.task_name).await;
    assert_eq!(result.nickname, None);

    let snapshot = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist")
        .config_snapshot()
        .await;
    assert_eq!(snapshot.sandbox_policy(), expected_sandbox);
    assert_eq!(snapshot.approval_policy, AskForApproval::OnRequest);
    assert_eq!(snapshot.permission_profile, expected_permission_profile);
    let child_thread = manager
        .get_thread(agent_id)
        .await
        .expect("spawned agent thread should exist");
    let child_turn = child_thread.codex.session.new_default_turn().await;
    assert_eq!(
        child_turn.file_system_sandbox_policy(),
        expected_file_system_sandbox_policy
    );
    assert_eq!(
        child_turn.network_sandbox_policy(),
        expected_network_sandbox_policy
    );
    assert_eq!(child_turn.permission_profile(), expected_permission_profile);
}

#[tokio::test]
async fn spawn_agent_inherits_runtime_permissions_without_a_role_profile() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager.start_thread((*turn.config).clone()).await.expect("start root");
    session.thread_id = root.thread_id;
    session.services.agent_control = manager.agent_control();
    let parent_permission_profile = PermissionProfile::workspace_write();
    turn.permission_profile = parent_permission_profile.clone();
    let expected_file_system_policy = turn.file_system_sandbox_policy();
    assert_ne!(parent_permission_profile, PermissionProfile::read_only());

    let output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "spawn_agent",
            function_payload(json!({"task_name": "test_agent",
                "message": "inspect this repo without editing it"
            })),
        ))
        .await
        .expect("generic spawn should preserve runtime permissions");
    let (content, _) = expect_text_output(output);
    let result: serde_json::Value =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    let agent_id = spawned_thread_id(
        &manager,
        result["task_name"].as_str().expect("spawn result should include task_name"),
    ).await;

    let child_thread = manager
        .get_thread(agent_id)
        .await
        .expect("spawned generic thread should exist");
    let snapshot = child_thread.config_snapshot().await;
    assert_eq!(snapshot.permission_profile, parent_permission_profile);
    assert!(matches!(
        snapshot.sandbox_policy(),
        SandboxPolicy::WorkspaceWrite { .. }
    ));
    let child_turn = child_thread.codex.session.new_default_turn().await;
    assert_eq!(
        child_turn.permission_profile(),
        parent_permission_profile
    );
    assert_eq!(
        child_turn.file_system_sandbox_policy(),
        expected_file_system_policy
    );
}





#[tokio::test]
async fn multi_agent_v2_spawn_agent_ignores_configured_max_depth() {
    #[derive(Debug, Deserialize)]
    struct SpawnAgentResult {
        task_name: String,
        nickname: Option<String>,
    }

    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config.agent_max_depth = 1;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    set_turn_config(&mut turn, config);
    let parent_path = AgentPath::try_from("/root/parent").expect("agent path");
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(parent_path),
        agent_nickname: None,
        agent_role: None,
    });

    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "spawn_agent",
        function_payload(json!({
            "message": "hello",
            "task_name": "child",
            "fork_turns": "none"
        })),
    );
    let output = SpawnAgentHandlerV2::default()
        .handle(invocation)
        .await
        .expect("multi-agent v2 spawn should ignore max depth");
    let (content, success) = expect_text_output(output);
    let result: SpawnAgentResult =
        serde_json::from_str(&content).expect("spawn_agent result should be json");
    assert_eq!(result.task_name, "/root/parent/child");
    assert_eq!(result.nickname, None);
    assert_eq!(success, Some(true));
}





































#[tokio::test]
async fn multi_agent_v2_wait_agent_accepts_timeout_only_argument() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let explicit_timeout_ms = turn.config.multi_agent_v2.min_wait_timeout_ms;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let worker_path = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .expect("worker metadata")
        .agent_path
        .expect("worker path");

    let wait_task = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            WaitAgentHandlerV2::default()
                .handle(invocation(
                    session,
                    turn,
                    "wait_agent",
                    function_payload(json!({"timeout_ms": explicit_timeout_ms})),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;

    let worker_label = worker_path.to_string();
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_path,
            AgentPath::root(),
            Vec::new(),
            "hello from worker".to_string(),
            /*trigger_turn*/ false,
        ))
        .await
        .expect("mailbox admission");

    let output = wait_task
        .await
        .expect("wait task should join")
        .expect("timeout-only args should be accepted in v2 mode");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    // A mailbox wake names its kind and the author of the queued message; it
    // is not a statement that the worker finished.
    assert_eq!(
        result.message,
        format!(
            "Wait woke on mailbox activity: messages from {worker_label} arrive with your next request."
        )
    );
    assert!(!result.timed_out);
    // Mail is an input-only exit: no durable page is delivered and the
    // caller's automatic cursor (unset for this fresh root) is not advanced.
    assert_eq!(result.cursor, None);
    assert!(result.typed_deltas.is_empty());
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_uses_owned_wait_below_configured_min() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 50;
    config.multi_agent_v2.max_wait_timeout_ms = 1_000;
    config.multi_agent_v2.default_wait_timeout_ms = 50;
    set_turn_config(&mut turn, config);

    let handler = WaitAgentHandlerV2::default();
    let waiting = handler.handle(invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({"timeout_ms": 1})),
    ));
    assert!(
        timeout(Duration::from_millis(120), waiting).await.is_err(),
        "below-minimum timeout must remain owned across maintenance intervals"
    );
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_accepts_explicit_timeout_at_configured_min() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 1;
    config.multi_agent_v2.max_wait_timeout_ms = 1_000;
    config.multi_agent_v2.default_wait_timeout_ms = 50;
    set_turn_config(&mut turn, config);

    let output = WaitAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "wait_agent",
            function_payload(json!({"timeout_ms": 1})),
        ))
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait timed out.".to_string(),
            timed_out: true,
            ..Default::default()
        }
    );
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_keeps_configured_default_timeout_internal() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 1;
    config.multi_agent_v2.max_wait_timeout_ms = 1_000;
    config.multi_agent_v2.default_wait_timeout_ms = 50;
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let early = timeout(
        Duration::from_millis(/*millis*/ 20),
        WaitAgentHandlerV2::default().handle(invocation(
            session.clone(),
            turn.clone(),
            "wait_agent",
            function_payload(json!({})),
        )),
    )
    .await;
    assert!(
        early.is_err(),
        "wait_agent should not return before the configured default timeout"
    );

    let after_multiple_maintenance_intervals = timeout(
        Duration::from_millis(/*millis*/ 120),
        WaitAgentHandlerV2::default().handle(invocation(
            session,
            turn,
            "wait_agent",
            function_payload(json!({})),
        )),
    )
    .await;
    assert!(
        after_multiple_maintenance_intervals.is_err(),
        "omitted timeout_ms must remain pending across default maintenance intervals"
    );
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_treats_zero_default_as_internal_cadence() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 0;
    config.multi_agent_v2.max_wait_timeout_ms = 0;
    config.multi_agent_v2.default_wait_timeout_ms = 0;
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let pending = timeout(
        Duration::from_millis(/*millis*/ 20),
        WaitAgentHandlerV2::default().handle(invocation(
            session,
            turn,
            "wait_agent",
            function_payload(json!({})),
        )),
    )
    .await;
    assert!(
        pending.is_err(),
        "an omitted timeout must not become a zero deadline"
    );
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_cancellation_wakes_immediately() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let invocation = invocation(
        Arc::new(session),
        Arc::new(turn),
        "wait_agent",
        function_payload(json!({})),
    );
    let cancellation_token = invocation.cancellation_token.clone();
    let wait = tokio::spawn(async move { WaitAgentHandlerV2::default().handle(invocation).await });
    tokio::task::yield_now().await;

    cancellation_token.cancel();

    let result = timeout(Duration::from_secs(2), wait)
        .await
        .expect("cancellation should wake the wait immediately")
        .expect("wait task should join");
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("cancelled wait should report cancellation"),
    };
    assert_eq!(
        error,
        FunctionCallError::RespondToModel("wait_agent cancelled".to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_clamps_timeout_above_configured_max() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 1;
    config.multi_agent_v2.max_wait_timeout_ms = 50;
    config.multi_agent_v2.default_wait_timeout_ms = 1;
    set_turn_config(&mut turn, config);

    let started = tokio::time::Instant::now();
    let handler = WaitAgentHandlerV2::default();
    let output = tokio::time::timeout(
        Duration::from_secs(5),
        handler.handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "wait_agent",
            function_payload(json!({"timeout_ms": 60_000})),
        )),
    )
    .await
    .expect("the configured maximum bounds the wait")
    .expect("oversized timeout is clamped");
    let (content, _) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait result");
    assert!(result.timed_out);
    assert!(started.elapsed() >= Duration::from_millis(50));
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_accepts_explicit_timeout_at_configured_max() {
    let (session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    config.multi_agent_v2.min_wait_timeout_ms = 1;
    config.multi_agent_v2.max_wait_timeout_ms = 1;
    config.multi_agent_v2.default_wait_timeout_ms = 1;
    set_turn_config(&mut turn, config);

    let output = WaitAgentHandlerV2::default()
        .handle(invocation(
            Arc::new(session),
            Arc::new(turn),
            "wait_agent",
            function_payload(json!({"timeout_ms": 1})),
        ))
        .await
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: "Wait timed out.".to_string(),
            timed_out: true,
            ..Default::default()
        }
    );
    assert_eq!(success, None);
}









#[tokio::test]
async fn multi_agent_v2_wait_agent_returns_summary_for_mailbox_activity() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let spawn_output = SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "test_process"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");
    let _ = expect_text_output(spawn_output);

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "test_process")
        .await
        .expect("relative path should resolve");
    let worker_path = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .expect("worker metadata")
        .agent_path
        .expect("worker path");
    let wait_task = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            WaitAgentHandlerV2::default()
                .handle(invocation(
                    session,
                    turn,
                    "wait_agent",
                    function_payload(json!({})),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;

    let worker_label = worker_path.to_string();
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_path,
            AgentPath::root(),
            Vec::new(),
            "sensitive child output".to_string(),
            /*trigger_turn*/ false,
        ))
        .await
        .expect("mailbox admission");

    let wait_output = wait_task
        .await
        .expect("wait task should join")
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(wait_output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result.message,
        format!(
            "Wait woke on mailbox activity: messages from {worker_label} arrive with your next request."
        )
    );
    assert!(!result.timed_out);
    // Mail is an input-only exit: no durable page is delivered and the
    // caller's automatic cursor (unset for this fresh root) is not advanced.
    assert_eq!(result.cursor, None);
    assert!(result.typed_deltas.is_empty());
    assert!(!content.contains("sensitive child output"));
    assert_eq!(success, None);
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_returns_for_already_queued_mail() {
    let (mut session, mut turn, rx) = make_session_and_context_with_rx().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    {
        let session = Arc::get_mut(&mut session).expect("session should be uniquely owned");
        session.services.agent_control = manager.agent_control();
        session.thread_id = root.thread_id;
    }
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(
        Arc::get_mut(&mut turn).expect("turn context should be uniquely owned"),
        config,
    );

    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "boot worker",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn worker");
    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker should resolve");
    let worker_path = session
        .services
        .agent_control
        .get_agent_metadata(agent_id)
        .expect("worker metadata")
        .agent_path
        .expect("worker path");

    let worker_label = worker_path.to_string();
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_path,
            AgentPath::root(),
            Vec::new(),
            "already queued".to_string(),
            /*trigger_turn*/ false,
        ))
        .await
        .expect("mailbox admission");

    let output = timeout(
        Duration::from_millis(500),
        WaitAgentHandlerV2::default().handle(invocation(
            session,
            turn,
            "wait_agent",
            function_payload(json!({})),
        )),
    )
    .await
    .expect("already queued mail should complete wait_agent immediately")
    .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert_eq!(
        result,
        crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult {
            message: format!(
                "Wait woke on mailbox activity: messages from {worker_label} arrive with your next request."
            ),
            timed_out: false,
            ..Default::default()
        }
    );
    assert_eq!(success, None);
    // The completed item names the agent whose message the wait delivered,
    // with its current state, as V1 wait items do.
    let item = completed_collab_item(&rx, "call-1").await;
    assert_eq!(item.status, CollabAgentToolCallStatus::Completed);
    assert_eq!(item.receiver_thread_ids, vec![agent_id]);
    assert_eq!(
        item.receiver_agents
            .iter()
            .map(|agent| agent.thread_id)
            .collect::<Vec<_>>(),
        vec![agent_id]
    );
    assert_eq!(item.agents_states.len(), 1);
    assert!(item.agents_states.contains_key(&agent_id));
}

#[tokio::test]
async fn multi_agent_v2_wait_agent_wakes_on_any_mailbox_notification() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    for task_name in ["worker_a", "worker_b"] {
        SpawnAgentHandlerV2::default()
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "spawn_agent",
                function_payload(json!({
                    "message": format!("boot {task_name}"),
                    "task_name": task_name
                })),
            ))
            .await
            .expect("spawn worker");
    }
    let worker_b_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker_b")
        .await
        .expect("worker_b should resolve");
    let worker_b_path = session
        .services
        .agent_control
        .get_agent_metadata(worker_b_id)
        .expect("worker_b metadata")
        .agent_path
        .expect("worker_b path");

    let wait_task = tokio::spawn({
        let session = session.clone();
        let turn = turn.clone();
        async move {
            WaitAgentHandlerV2::default()
                .handle(invocation(
                    session,
                    turn,
                    "wait_agent",
                    function_payload(json!({})),
                ))
                .await
        }
    });
    tokio::task::yield_now().await;

    let worker_b_label = worker_b_path.to_string();
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            worker_b_path,
            AgentPath::root(),
            Vec::new(),
            "from worker b".to_string(),
            /*trigger_turn*/ false,
        ))
        .await
        .expect("mailbox admission");

    let output = wait_task
        .await
        .expect("wait task should join")
        .expect("wait_agent should succeed");
    let (content, success) = expect_text_output(output);
    let result: crate::tools::handlers::multi_agents_v2::wait::WaitAgentResult =
        serde_json::from_str(&content).expect("wait_agent result should be json");
    assert!(
        result
            .message
            .starts_with("Wait woke on mailbox activity: messages from ")
            && result.message.contains(&worker_b_label),
        "{}",
        result.message
    );
    assert!(!result.timed_out);
    // Any child's mail wakes the wait as an input-only exit: no durable page is
    // delivered and the automatic cursor is not advanced.
    assert_eq!(result.cursor, None);
    assert!(result.typed_deltas.is_empty());
    assert_eq!(success, None);

    // The workers' durable events stay retained behind the unchanged automatic
    // cursor. Their admission alone no longer wakes a later wait, so inspect the
    // retained stream directly.
    let coordinator = session.services.agent_control.task_coordinator();
    let store = coordinator.store().expect("spawn initialized the task store");
    let root_session_id = coordinator.root_session_id().expect("root session");
    let consumer = turn
        .session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root)
        .to_string();
    let cursor = store
        .automatic_wake_cursor(root_session_id.clone(), consumer)
        .await
        .expect("automatic cursor");
    let retained = store
        .read_wake_events(root_session_id, cursor)
        .await
        .expect("retained wake events");
    for task_name in ["worker_a", "worker_b"] {
        let agent_path = AgentPath::root().join(task_name).expect("worker path");
        let binding = coordinator
            .binding_for_agent_path(&agent_path)
            .expect("worker assignment");
        assert!(
            retained
                .updated_agents
                .iter()
                .any(|event| event.assignment_id == binding.assignment_id),
            "the mailbox exit must not consume {task_name}'s durable events"
        );
    }
}

#[tokio::test]
async fn multi_agent_v2_interrupt_agent_accepts_task_name_target() {
    let (mut session, mut turn) = make_session_and_context().await;
    // Spawned agents inherit the caller's environment leases. As in production,
    // those leases come from the agents' own environment manager; a lease from
    // another manager reads as replaced and is never retargeted.
    let manager_home = tempfile::tempdir().expect("thread manager home");
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        built_in_model_providers(/*openai_base_url*/ None)["openai"].clone(),
        manager_home.path().to_path_buf(),
        session.services.turn_environments.environment_manager(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    root.thread.codex.session.new_default_turn().await;
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    let worker_thread = manager
        .get_thread(agent_id)
        .await
        .expect("worker thread should be resident");
    let worker_session = worker_thread.codex.session.clone();
    SpawnAgentHandlerV2::default()
        .handle(invocation(
            worker_session.clone(),
            worker_session.new_default_turn().await,
            "spawn_agent",
            function_payload(json!({
                "message": "inspect a child task",
                "task_name": "child"
            })),
        ))
        .await
        .expect("child spawn should succeed");
    let child_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker/child")
        .await
        .expect("child path should resolve");

    let output = InterruptAgentHandler
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "interrupt_agent",
            function_payload(json!({"target": "worker"})),
        ))
        .await
        .expect("interrupt_agent should succeed for v2 task names");
    let (content, success) = expect_text_output(output);
    let result: InterruptAgentResult =
        serde_json::from_str(&content).expect("interrupt_agent result should be json");
    assert_ne!(result.previous_status, AgentStatus::NotFound);
    assert_eq!(success, Some(true));
    assert_eq!(
        session
            .services
            .agent_control
            .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
            .await
            .expect("worker path should remain resolvable"),
        agent_id
    );
    manager
        .get_thread(agent_id)
        .await
        .expect("worker should remain resident");
    manager
        .get_thread(child_id)
        .await
        .expect("child should remain resident");
    let ops = manager.captured_ops();
    assert!(
        ops.iter()
            .any(|(thread_id, op)| *thread_id == agent_id && matches!(op, Op::Interrupt))
    );
    assert!(!ops.iter().any(|(thread_id, op)| {
        (*thread_id == agent_id || *thread_id == child_id) && matches!(op, Op::Shutdown)
    }));
    assert!(
        !ops.iter()
            .any(|(thread_id, op)| *thread_id == child_id && matches!(op, Op::Interrupt))
    );
}

#[tokio::test]
async fn multi_agent_v2_interrupt_agent_accepts_unloaded_task_name_target() {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    let state_db = init_state_db(&config)
        .await
        .expect("sqlite state db should initialize");
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_db.clone()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
    );
    let root = manager
        .start_thread(config.clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    set_turn_config(&mut turn, config.clone());

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    SpawnAgentHandlerV2::default()
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "spawn_agent",
            function_payload(json!({
                "message": "inspect this repo",
                "task_name": "worker"
            })),
        ))
        .await
        .expect("spawn_agent should succeed");

    let agent_id = session
        .services
        .agent_control
        .resolve_agent_reference(session.thread_id, &turn.session_source, "worker")
        .await
        .expect("worker path should resolve");
    let stale_thread = manager
        .remove_thread(&agent_id)
        .await
        .expect("worker thread should be loaded before removal");
    stale_thread
        .submit(Op::Shutdown {})
        .await
        .expect("removed worker thread should still accept shutdown");
    stale_thread.wait_until_terminated().await;

    let output = InterruptAgentHandler
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "interrupt_agent",
            function_payload(json!({"target": "worker"})),
        ))
        .await
        .expect("interrupt_agent should accept unloaded v2 task names");
    let (content, success) = expect_text_output(output);
    let result: InterruptAgentResult =
        serde_json::from_str(&content).expect("interrupt_agent result should be json");
    assert_eq!(result.previous_status, AgentStatus::NotFound);
    assert_eq!(success, Some(true));

    let open_children = state_db
        .list_thread_spawn_children_with_status(
            root.thread_id,
            DirectionalThreadSpawnEdgeStatus::Open,
        )
        .await
        .expect("open children should load");
    assert_eq!(open_children, vec![agent_id]);
    let closed_children = state_db
        .list_thread_spawn_children_with_status(
            root.thread_id,
            DirectionalThreadSpawnEdgeStatus::Closed,
        )
        .await
        .expect("closed children should load");
    assert_eq!(closed_children, Vec::<ThreadId>::new());

    let output = ListAgentsHandlerV2
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "list_agents",
            function_payload(json!({})),
        ))
        .await
        .expect("list_agents should succeed");
    let (content, _) = expect_text_output(output);
    let codex_tools::ToolSpec::Function(list_spec) = ListAgentsHandlerV2.spec() else {
        panic!("list_agents should be a function tool");
    };
    let schema = list_spec
        .output_schema
        .expect("list_agents output schema")
        .into_value();
    let listed: serde_json::Value =
        serde_json::from_str(&content).expect("list_agents result should be json");
    let schema_errors = jsonschema::validator_for(&schema)
        .expect("compile list_agents output schema")
        .iter_errors(&listed)
        .map(|error| error.to_string())
        .collect::<Vec<_>>();
    assert!(
        schema_errors.is_empty(),
        "list_agents output must match its advertised schema: {schema_errors:?}"
    );
    let result: ListAgentsResult =
        serde_json::from_str(&content).expect("list_agents result should be json");
    assert_eq!(result.agents.len(), 2);
    assert_eq!(result.agents[0].agent_name, "/root");
    assert!(result.agents[0].runtime_loaded);
    assert_eq!(result.agents[1].agent_name, "/root/worker");
    assert!(!result.agents[1].runtime_loaded);
}

#[tokio::test]
async fn multi_agent_v2_interrupt_agent_rejects_root_target_and_id() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let root_path_error = InterruptAgentHandler
        .handle(invocation(
            session.clone(),
            turn.clone(),
            "interrupt_agent",
            function_payload(json!({"target": "/root"})),
        ))
        .await
        .err()
        .expect("interrupt_agent should reject the root path");
    assert_eq!(
        root_path_error,
        FunctionCallError::RespondToModel("root is not a spawned agent".to_string())
    );

    let root_id_error = InterruptAgentHandler
        .handle(invocation(
            session,
            turn,
            "interrupt_agent",
            function_payload(json!({"target": root.thread_id.to_string()})),
        ))
        .await
        .err()
        .expect("interrupt_agent should reject the root thread id");
    assert_eq!(
        root_id_error,
        FunctionCallError::RespondToModel("root is not a spawned agent".to_string())
    );
}

#[tokio::test]
async fn multi_agent_v2_interrupt_agent_rejects_self_target_by_id() {
    let (mut session, mut turn) = make_session_and_context().await;
    let manager = thread_manager();
    let mut config = (*turn.config).clone();
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
    set_turn_config(&mut turn, config);
    let root = manager
        .start_thread((*turn.config).clone())
        .await
        .expect("root thread should start");
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;

    let child_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_thread_id = session
        .services
        .agent_control
        .spawn_agent_with_metadata(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "inspect this repo".to_string(),
                text_elements: Vec::new(),
            }],
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root.thread_id,
                depth: 1,
                agent_path: Some(child_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            crate::agent::control::SpawnAgentOptions::default(),
        )
        .await
        .expect("worker spawn should succeed")
        .thread_id;
    session.thread_id = child_thread_id;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root.thread_id,
        depth: 1,
        agent_path: Some(child_path.clone()),
        agent_nickname: None,
        agent_role: None,
    });

    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let before = manager.captured_ops().len();
    for target in [child_thread_id.to_string(), child_path.to_string()] {
        let err = InterruptAgentHandler
            .handle(invocation(
                session.clone(),
                turn.clone(),
                "interrupt_agent",
                function_payload(json!({"target": target})),
            ))
            .await
            .err()
            .expect("interrupt_agent should reject self-target by id");
        assert_eq!(
            err,
            FunctionCallError::RespondToModel(
                "an agent cannot interrupt itself; return your result and let the parent interrupt you if needed"
                    .to_string()
            )
        );
    }
    assert_eq!(manager.captured_ops().len(), before);
}







#[tokio::test]
async fn build_agent_spawn_config_uses_turn_context_values() {
    fn pick_allowed_sandbox_policy(
        permissions: &crate::config::Permissions,
        base: SandboxPolicy,
        cwd: &std::path::Path,
    ) -> SandboxPolicy {
        let candidates = [
            SandboxPolicy::new_read_only_policy(),
            SandboxPolicy::new_workspace_write_policy(),
            SandboxPolicy::DangerFullAccess,
        ];
        candidates
            .into_iter()
            .find(|candidate| {
                if *candidate == base {
                    return false;
                }
                permissions
                    .can_set_legacy_sandbox_policy(candidate, cwd)
                    .is_ok()
            })
            .unwrap_or(base)
    }

    let (_session, mut turn) = make_session_and_context().await;
    turn.base_instructions = Arc::new(BaseInstructions {
        text: "active turn base".to_string(),
    });
    turn.developer_instructions = Some("dev".to_string());
    let mut config = (*turn.config).clone();
    config.compact_prompt = Some("compact".to_string());
    config.permissions.shell_environment_policy = ShellEnvironmentPolicy {
        use_profile: true,
        ..ShellEnvironmentPolicy::default()
    };
    let temp_dir = tempfile::tempdir().expect("temp dir");
    config.cwd = temp_dir.abs();
    turn.config = Arc::new(config);
    let turn_cwd = turn.cwd().clone();
    let sandbox_policy = pick_allowed_sandbox_policy(
        &turn.config.permissions,
        turn.config.legacy_sandbox_policy(),
        turn_cwd.as_path(),
    );
    let file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy_for_cwd(&sandbox_policy, &turn_cwd);
    let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
    let permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
        SandboxEnforcement::from_legacy_sandbox_policy(&sandbox_policy),
        &file_system_sandbox_policy,
        network_sandbox_policy,
    );
    turn.permission_profile = permission_profile.clone();
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");

    let config = build_agent_spawn_config(&turn).expect("spawn config");
    let mut expected = (*turn.config).clone();
    expected.base_instructions = Some(turn.base_instructions.text.clone());
    expected.model = Some(turn.model_info.slug.clone());
    expected.model_provider = turn.provider.info().clone();
    expected.model_reasoning_effort = turn
        .reasoning_effort
        .clone()
        .or_else(|| turn.model_info.default_reasoning_level.clone());
    expected.model_reasoning_summary = Some(turn.reasoning_summary);
    expected.developer_instructions = turn.developer_instructions.clone();
    expected.cwd = turn.cwd().clone();
    expected
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");
    expected
        .permissions
        .set_permission_profile(permission_profile)
        .expect("permission profile set");
    assert_eq!(config, expected);
}

#[tokio::test]
async fn live_runtime_windows_sandbox_is_projected_into_child_config() {
    let (_session, mut turn) = make_session_and_context().await;
    let mut turn_config = (*turn.config).clone();
    turn_config
        .features
        .enable(Feature::WindowsSandbox)
        .expect("legacy restricted-token feature should be enabled");
    turn_config
        .features
        .enable(Feature::WindowsSandboxElevated)
        .expect("legacy elevated feature should be enabled");
    turn.config = Arc::new(turn_config);
    for level in [
        WindowsSandboxLevel::Disabled,
        WindowsSandboxLevel::RestrictedToken,
        WindowsSandboxLevel::Elevated,
    ] {
        turn.windows_sandbox_level = level;

        let child_config = build_agent_spawn_config(&turn).expect("spawn config");

        assert_eq!(WindowsSandboxLevel::from_config(&child_config), level);
    }
}

#[tokio::test]
async fn kd4_runtime_control_is_inherited_by_spawned_and_resumed_agents() {
    let (_session, mut turn) = make_session_and_context().await;
    for enabled in [false, true] {
        let mut parent = (*turn.config).clone();
        parent
            .features
            .set_enabled(Feature::Kd4Runtime, enabled)
            .expect("set runtime control");
        turn.config = Arc::new(parent);
        for child in [
            build_agent_spawn_config(&turn).expect("spawn config"),
            build_agent_resume_config(&turn).expect("resume config"),
        ] {
            assert_eq!(child.features.enabled(Feature::Kd4Runtime), enabled);
        }
    }
}

#[tokio::test]
async fn build_agent_resume_config_clears_base_instructions() {
    let (_session, mut turn) = make_session_and_context().await;
    let mut base_config = (*turn.config).clone();
    base_config.base_instructions = Some("caller-base".to_string());
    turn.config = Arc::new(base_config);
    turn.approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");

    let config = build_agent_resume_config(&turn).expect("resume config");

    let mut expected = (*turn.config).clone();
    expected.base_instructions = None;
    expected.model = Some(turn.model_info.slug.clone());
    expected.model_provider = turn.provider.info().clone();
    expected.model_reasoning_effort = turn
        .reasoning_effort
        .clone()
        .or_else(|| turn.model_info.default_reasoning_level.clone());
    expected.model_reasoning_summary = Some(turn.reasoning_summary);
    expected.developer_instructions = turn.developer_instructions.clone();
    expected.cwd = turn.cwd().clone();
    expected
        .permissions
        .approval_policy
        .set(AskForApproval::OnRequest)
        .expect("approval policy set");
    expected
        .permissions
        .set_permission_profile(turn.permission_profile())
        .expect("permission profile set");
    assert_eq!(config, expected);
}




#[tokio::test]
async fn compatibility_reviewer_is_rejected_before_admission_or_child_creation() {
    let (mut session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    config.features.enable(Feature::MultiAgentV2).unwrap();
    let manager = thread_manager();
    let root = manager.start_thread(config.clone()).await.unwrap();
    session.services.agent_control = manager.agent_control();
    session.thread_id = root.thread_id;
    let control = session.services.agent_control.clone();
    set_turn_config(&mut turn, config);
    let result = SpawnAgentHandlerV2::default().handle(invocation(
    Arc::new(session), Arc::new(turn), "spawn_agent",
    function_payload(json!({"task_name":"review", "agent_type":"reviewer", "message":"review this change"})),
)).await;
    let error = result.err().expect("message-form reviewer must fail");
    assert!(
        error
            .to_string()
            .contains("specialized roles are no longer supported")
    );
    assert_eq!(manager.list_thread_ids().await, vec![root.thread_id]);
    assert!(
        control
            .task_coordinator()
            .bindings_for_recovery()
            .is_empty()
    );
    assert!(manager.captured_ops().is_empty());
    manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
}



#[tokio::test]
async fn compact_uuid_leaf_routes_to_the_named_child_not_the_other_live_thread() {
    let (mut session, turn) = make_session_and_context().await;
    let manager = thread_manager();
    let root = manager.start_thread((*turn.config).clone()).await.unwrap();
    let control = manager.agent_control();
    control.register_session_root(root.thread_id, None);
    let other = control
        .spawn_agent(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "other child".into(),
                text_elements: vec![],
            }],
            Some(
                thread_spawn_source(
                    root.thread_id,
                    &SessionSource::Exec,
                    1,
                    Some("worker"),
                    Some("other".into()),
                )
                .unwrap(),
            ),
        )
        .await
        .unwrap();
    let leaf = other.to_string().replace('-', "");
    let named = control
        .spawn_agent(
            (*turn.config).clone(),
            vec![UserInput::Text {
                text: "named child".into(),
                text_elements: vec![],
            }],
            Some(
                thread_spawn_source(
                    root.thread_id,
                    &SessionSource::Exec,
                    1,
                    Some("worker"),
                    Some(leaf.clone()),
                )
                .unwrap(),
            ),
        )
        .await
        .unwrap();
    session.thread_id = root.thread_id;
    session.services.agent_control = control;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    for target in [leaf.clone(), format!("/root/{leaf}")] {
        assert_eq!(
            crate::agent::agent_resolver::resolve_agent_target(&session, &turn, &target)
                .await
                .unwrap(),
            named
        );
    }
    assert_eq!(
        crate::agent::agent_resolver::resolve_agent_target(&session, &turn, &other.to_string())
            .await
            .unwrap(),
        other
    );
    manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
}


async fn spawned_thread_id(manager: &ThreadManager, task_name: &str) -> ThreadId {
    for thread_id in manager.list_thread_ids().await {
        if manager.agent_control().get_agent_metadata(thread_id)
            .and_then(|metadata| metadata.agent_path)
            .is_some_and(|path| path.as_str() == task_name)
        {
            return thread_id;
        }
    }
    panic!("spawned task {task_name} has no live thread");
}
