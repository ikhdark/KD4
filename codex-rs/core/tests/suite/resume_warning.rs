#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use codex_core::NewThread;
use codex_login::CodexAuth;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMeta;
use codex_protocol::protocol::SessionMetaLine;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnContextItem;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::load_default_config_for_test;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use tempfile::TempDir;

fn resume_history(
    config: &codex_core::config::Config,
    previous_model: &str,
    rollout_path: &std::path::Path,
) -> InitialHistory {
    let conversation_id = ThreadId::new();
    let session_meta = RolloutLine {
        timestamp: "2024-01-01T00:00:00.000Z".to_string(),
        item: RolloutItem::SessionMeta(SessionMetaLine {
            meta: SessionMeta {
                session_id: conversation_id.into(),
                id: conversation_id,
                parent_thread_id: None,
                timestamp: "2024-01-01T00:00:00Z".to_string(),
                cwd: config.cwd.to_path_buf(),
                originator: "resume_warning_test".to_string(),
                cli_version: "test_version".to_string(),
                model_provider: Some(config.model_provider_id.clone()),
                ..Default::default()
            },
            git: None,
        }),
    };
    let session_meta = serde_json::to_string(&session_meta).expect("serialize session metadata");
    std::fs::write(rollout_path, format!("{session_meta}\n"))
        .expect("write valid rollout metadata");

    let turn_id = "resume-warning-seed-turn".to_string();
    let turn_ctx = TurnContextItem {
        turn_id: Some(turn_id.clone()),
        cwd: config.cwd.clone(),
        workspace_roots: None,
        current_date: None,
        timezone: None,
        approval_policy: config.permissions.approval_policy.value(),
        approvals_reviewer: None,
        sandbox_policy: config.legacy_sandbox_policy(),
        permission_profile: None,
        network: None,
        file_system_sandbox_policy: None,
        model: previous_model.to_string(),
        comp_hash: None,
        personality: None,
        collaboration_mode: None,
        multi_agent_version: None,
        multi_agent_mode: None,
        effort: config.model_reasoning_effort.clone(),
        context_provenance: None,
    };

    InitialHistory::Resumed(ResumedHistory {
        conversation_id,
        history: Arc::new(vec![
            RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: ModeKind::Default,
            })),
            RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                client_id: None,
                message: "seed".to_string(),
                images: None,
                local_images: vec![],
                text_elements: vec![],
                ..Default::default()
            })),
            RolloutItem::TurnContext(turn_ctx),
            RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                surfaced_result: None,
                turn_id,
                last_agent_message: None,
                error: None,
                completion: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                timing: None,
            })),
        ]),
        rollout_path: Some(rollout_path.to_path_buf()),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resumed_model_difference_uses_model_switch_context_without_legacy_warning() {
    skip_if_no_network!();

    let server = start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    // Arrange a config with a current model and a prior rollout recorded under a different model.
    let home = TempDir::new().expect("tempdir");
    let mut config = load_default_config_for_test(&home).await;
    config.model = Some("gpt-5.4".to_string());
    config.model_provider.base_url = Some(format!("{}/v1", server.uri()));
    config.model_provider.supports_websockets = false;
    config.model_providers.insert(
        config.model_provider_id.clone(),
        config.model_provider.clone(),
    );
    // Ensure cwd is absolute (the helper sets it to the temp dir already).
    assert!(config.cwd.is_absolute());

    let rollout_path = home.path().join("rollout.jsonl");
    let initial_history = resume_history(&config, "gpt-5.2", &rollout_path);

    let thread_manager = codex_core::test_support::thread_manager_with_models_provider(
        CodexAuth::from_api_key("test"),
        config.model_provider.clone(),
    );
    let auth_manager =
        codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key("test"));

    // Act: resume the conversation.
    let NewThread {
        thread: conversation,
        ..
    } = thread_manager
        .resume_thread_with_history(
            config.clone(),
            initial_history,
            auth_manager,
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await
        .expect("resume conversation");

    conversation
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "continue".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                model: Some("gpt-5.4".to_string()),
                ..Default::default()
            },
        })
        .await
        .expect("submit resumed turn");

    let mut legacy_warning = None;
    let mut turn_error = None;
    loop {
        let event = conversation.next_event().await.expect("next event");
        match event.msg {
            EventMsg::Warning(WarningEvent { message })
                if message.contains("gpt-5.2") && message.contains("gpt-5.4") =>
            {
                legacy_warning = Some(message);
            }
            EventMsg::Error(error) => turn_error = Some(error.message),
            EventMsg::TurnComplete(_) => break,
            _ => {}
        }
    }
    assert_eq!(
        legacy_warning, None,
        "legacy resume warnings are no longer emitted"
    );
    assert_eq!(
        turn_error, None,
        "resumed turn should reach the model provider"
    );

    let developer_texts = response_mock
        .single_request()
        .message_input_texts("developer");
    assert!(
        developer_texts
            .iter()
            .any(|text| text.contains("<model_switch>") && text.contains("different model")),
        "the resumed request must carry the current model-switch contract: {developer_texts:?}"
    );
}
