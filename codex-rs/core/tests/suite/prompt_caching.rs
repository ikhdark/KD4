#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use codex_core::shell::default_user_shell;
use codex_features::Feature;
use codex_prompts::APPLY_PATCH_TOOL_INSTRUCTIONS;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Settings;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::ENVIRONMENT_CONTEXT_OPEN_TAG;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::user_input::UserInput;
use core_test_support::TempDirExt;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_metadata_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn write_global_instructions(home: &Path) {
    fs::write(home.join("AGENTS.md"), "be consistent and helpful")
        .expect("write global instructions");
}

fn text_user_input(text: String) -> serde_json::Value {
    text_user_input_parts(vec![text])
}

fn text_user_input_parts(texts: Vec<String>) -> serde_json::Value {
    serde_json::json!({
        "type": "message",
        "role": "user",
        "content": texts
            .into_iter()
            .map(|text| serde_json::json!({ "type": "input_text", "text": text }))
            .collect::<Vec<_>>()
    })
}

fn assert_eq_without_metadata(left: serde_json::Value, right: serde_json::Value) {
    assert_eq!(
        strip_metadata_from_json(left),
        strip_metadata_from_json(right)
    );
}

fn persisted_reasoning_items(path: &Path) -> Vec<ResponseItem> {
    fs::read_to_string(path)
        .expect("read rollout")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let rollout_line: RolloutLine = serde_json::from_str(line).expect("parse rollout line");
            match rollout_line.item {
                RolloutItem::ResponseItem(item @ ResponseItem::Reasoning { .. }) => Some(item),
                _ => None,
            }
        })
        .collect()
}

fn assert_default_env_context(text: &str, cwd: &str) {
    assert_env_context_fragment(text);
    assert!(
        text.contains(&format!("<cwd>{cwd}</cwd>")),
        "expected cwd in environment context: {text}"
    );
    assert!(
        text.contains(&format!("<shell>{}</shell>", default_user_shell().name())),
        "expected shell in environment context: {text}"
    );
}

fn assert_env_context_fragment(text: &str) {
    assert!(
        text.starts_with(ENVIRONMENT_CONTEXT_OPEN_TAG),
        "expected environment context fragment: {text}"
    );
    assert!(
        text.contains("<current_date>") && text.contains("</current_date>"),
        "expected current_date in environment context: {text}"
    );
    assert!(
        text.contains("<timezone>") && text.contains("</timezone>"),
        "expected timezone in environment context: {text}"
    );
    assert!(
        text.ends_with("</environment_context>"),
        "expected closing environment_context tag: {text}"
    );
}

fn assert_tool_names(body: &serde_json::Value, expected_names: &[&str]) {
    assert_eq!(
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                t.get("name")
                    .and_then(|value| value.as_str())
                    .or_else(|| t.get("type").and_then(|value| value.as_str()))
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>(),
        expected_names
    );
}

fn message_texts<'a>(body: &'a serde_json::Value, role: &str) -> Vec<&'a str> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item.get("role").and_then(serde_json::Value::as_str) == Some(role))
        .filter_map(|item| item.get("content").and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
        .collect()
}

fn environment_contexts(body: &serde_json::Value) -> Vec<&str> {
    message_texts(body, "user")
        .into_iter()
        .filter(|text| text.starts_with(ENVIRONMENT_CONTEXT_OPEN_TAG))
        .collect()
}

fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prompt_tools_are_consistent_across_requests() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    use pretty_assertions::assert_eq;

    let server = start_mock_server().await;
    let req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let TestCodex {
        codex,
        config,
        thread_manager,
        ..
    } = test_codex()
        .with_pre_build_hook(write_global_instructions)
        .with_config(|config| {
            config.model = Some("gpt-5.2".to_string());
            // Keep tool expectations stable when the default web_search mode changes.
            config
                .web_search_mode
                .set(WebSearchMode::Cached)
                .expect("test web_search_mode should satisfy constraints");
            config
                .features
                .enable(Feature::CollaborationModes)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;
    let base_instructions = thread_manager
        .get_models_manager()
        .get_model_info(
            config
                .model
                .as_deref()
                .expect("test config should have a model"),
            &config.to_models_manager_config(),
        )
        .await
        .base_instructions;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 1".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 2".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let expected_tools_names = vec![
        "exec",
        "exec_command",
        "write_stdin",
        "read_tool_output",
        "read_turn_timing",
        "update_plan",
        "request_user_input",
        "request_permissions",
        "apply_patch",
        "view_image",
        "tool_search",
        "web_search",
    ];
    let body0 = req1.single_request().body_json();

    let expected_instructions = if expected_tools_names.contains(&"apply_patch") {
        base_instructions
    } else {
        [base_instructions, APPLY_PATCH_TOOL_INSTRUCTIONS.to_string()].join("\n")
    };

    assert_eq!(
        body0["instructions"],
        serde_json::json!(expected_instructions),
    );
    assert_tool_names(&body0, &expected_tools_names);

    let body1 = req2.single_request().body_json();
    assert_eq!(
        body1["instructions"],
        serde_json::json!(expected_instructions),
    );
    assert_tool_names(&body1, &expected_tools_names);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gpt_5_tools_without_apply_patch_append_apply_patch_instructions() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    use pretty_assertions::assert_eq;

    let server = start_mock_server().await;
    let req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_pre_build_hook(write_global_instructions)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CollaborationModes)
                .expect("test config should allow feature update");
            config.model = Some("gpt-5.2".to_string());
        })
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 1".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 2".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let body0 = req1.single_request().body_json();
    let instructions0 = body0["instructions"]
        .as_str()
        .expect("instructions should be a string");
    assert!(
        instructions0.contains("You are"),
        "expected non-empty instructions"
    );

    let body1 = req2.single_request().body_json();
    let instructions1 = body1["instructions"]
        .as_str()
        .expect("instructions should be a string");
    assert_eq!(
        normalize_newlines(instructions1),
        normalize_newlines(instructions0)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefixes_context_and_instructions_once_and_consistently_across_requests()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    use pretty_assertions::assert_eq;

    let server = start_mock_server().await;
    let req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let TestCodex { codex, config, .. } = test_codex()
        .with_pre_build_hook(write_global_instructions)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CollaborationModes)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 1".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 2".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let body1 = req1.single_request().body_json();
    let input1 = body1["input"].as_array().expect("input array");
    let user_texts = message_texts(&body1, "user");
    let ui_text = user_texts
        .iter()
        .find(|text| text.starts_with("# AGENTS.md instructions"))
        .expect("AGENTS.md context text");
    assert!(
        ui_text.contains("be consistent and helpful"),
        "expected user instructions in UI message: {ui_text}"
    );

    let cwd_str = config.cwd.to_string_lossy();
    let env_contexts = environment_contexts(&body1);
    assert_eq!(env_contexts.len(), 1);
    let env_text = env_contexts[0];
    assert_default_env_context(env_text, &cwd_str);
    assert!(user_texts.contains(&"hello 1"));

    let body2 = req2.single_request().body_json();
    let input2 = body2["input"].as_array().expect("input array");
    assert_eq_without_metadata(
        serde_json::Value::Array(input2[..input1.len()].to_vec()),
        serde_json::Value::Array(input1.to_vec()),
    );
    assert_eq_without_metadata(
        input2[input1.len()].clone(),
        text_user_input("hello 2".to_string()),
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn overrides_turn_context_but_keeps_cached_prefix_and_key_constant() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    use pretty_assertions::assert_eq;

    let server = start_mock_server().await;
    let req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let TestCodex { codex, config, .. } = test_codex()
        .with_pre_build_hook(write_global_instructions)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CollaborationModes)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    // First turn
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 1".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let writable = TempDir::new().unwrap();
    let permission_profile = PermissionProfile::workspace_write_with(
        &[writable.abs()],
        NetworkSandboxPolicy::Enabled,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    );
    let sandbox_policy = permission_profile
        .to_legacy_sandbox_policy(config.cwd.as_path())
        .expect("workspace profile should have legacy projection");
    core_test_support::submit_thread_settings(
        &codex,
        codex_protocol::protocol::ThreadSettingsOverrides {
            approval_policy: Some(AskForApproval::Never),
            sandbox_policy: Some(sandbox_policy),
            permission_profile: Some(permission_profile),
            effort: Some(Some(ReasoningEffort::High)),
            summary: Some(ReasoningSummary::Detailed),
            ..Default::default()
        },
    )
    .await?;

    // Second turn after overrides
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 2".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request1 = req1.single_request();
    let request2 = req2.single_request();
    let body1 = request1.body_json();
    let body2 = request2.body_json();
    // prompt_cache_key should remain constant across overrides
    assert_eq!(
        body1["prompt_cache_key"], body2["prompt_cache_key"],
        "prompt_cache_key should not change across overrides"
    );

    let first_permissions = message_texts(&body1, "developer")
        .into_iter()
        .find(|text| text.starts_with("<permissions instructions>"))
        .expect("initial permissions context");
    let second_permissions = message_texts(&body2, "developer")
        .into_iter()
        .find(|text| text.starts_with("<permissions instructions>"))
        .expect("updated permissions context");
    assert_ne!(first_permissions, second_permissions);

    let env_contexts = environment_contexts(&body2);
    assert_eq!(env_contexts.len(), 1);
    let env_text = env_contexts[0];
    assert_env_context_fragment(env_text);
    assert!(
        env_text.contains("<permission_profile type=\"managed\">")
            && env_text.contains("<file_system type=\"restricted\">")
            && env_text.contains(&format!(
                "<entry access=\"write\"><path>{}</path></entry>",
                writable.abs().display()
            )),
        "expected workspace-write filesystem profile in environment context: {env_text}"
    );
    let second_user_texts = message_texts(&body2, "user");
    assert!(second_user_texts.contains(&"hello 1"));
    assert!(second_user_texts.contains(&"hello 2"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn override_before_first_turn_emits_environment_context() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex().build(&server).await?;

    let collaboration_mode = CollaborationMode {
        mode: ModeKind::Default,
        settings: Settings {
            model: "gpt-5.4".to_string(),
            reasoning_effort: Some(ReasoningEffort::High),
            developer_instructions: None,
        },
    };

    core_test_support::submit_thread_settings(
        &codex,
        codex_protocol::protocol::ThreadSettingsOverrides {
            approval_policy: Some(AskForApproval::Never),
            model: Some("gpt-5.4".to_string()),
            effort: Some(Some(ReasoningEffort::Low)),
            collaboration_mode: Some(collaboration_mode),
            ..Default::default()
        },
    )
    .await?;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first message".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;

    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let body = req.single_request().body_json();
    assert_eq!(body["model"].as_str(), Some("gpt-5.4"));
    assert_eq!(
        body.get("reasoning")
            .and_then(|reasoning| reasoning.get("effort"))
            .and_then(|value| value.as_str()),
        Some("high")
    );
    let input = body["input"]
        .as_array()
        .expect("input array must be present");
    assert!(
        !input.is_empty(),
        "expected at least environment context and user message"
    );

    let env_texts: Vec<&str> = input
        .iter()
        .filter_map(|msg| {
            msg["content"].as_array().map(|content| {
                content
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .filter(|text| text.starts_with(ENVIRONMENT_CONTEXT_OPEN_TAG))
        .collect();
    assert!(
        !env_texts.is_empty(),
        "expected environment context to be emitted: {env_texts:?}"
    );
    assert!(
        env_texts
            .iter()
            .any(|text| text.contains("<current_date>") && text.contains("</current_date>")),
        "expected current_date in environment context: {env_texts:?}"
    );
    assert!(
        env_texts
            .iter()
            .any(|text| text.contains("<timezone>") && text.contains("</timezone>")),
        "expected timezone in environment context: {env_texts:?}"
    );

    let env_count = input
        .iter()
        .filter(|msg| {
            msg["content"]
                .as_array()
                .and_then(|content| {
                    content.iter().find(|item| {
                        item["type"].as_str() == Some("input_text")
                            && item["text"]
                                .as_str()
                                .map(|text| text.starts_with(ENVIRONMENT_CONTEXT_OPEN_TAG))
                                .unwrap_or(false)
                    })
                })
                .is_some()
        })
        .count();
    assert!(
        env_count >= 1,
        "environment context should appear at least once, found {env_count}"
    );

    let permissions_texts: Vec<&str> = input
        .iter()
        .filter_map(|msg| {
            let role = msg["role"].as_str()?;
            if role != "developer" {
                return None;
            }
            msg["content"]
                .as_array()
                .and_then(|content| content.first())
                .and_then(|item| item["text"].as_str())
        })
        .collect();
    assert!(
        permissions_texts.iter().any(|text| {
            let lower = text.to_ascii_lowercase();
            (lower.contains("approval policy") || lower.contains("approval_policy"))
                && lower.contains("never")
        }),
        "permissions message should reflect overridden approval policy: {permissions_texts:?}"
    );

    let user_texts: Vec<&str> = input
        .iter()
        .filter_map(|msg| {
            msg["content"].as_array().map(|content| {
                content
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect();
    assert!(
        user_texts.contains(&"first message"),
        "expected user message text, got {user_texts:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn per_turn_overrides_keep_cached_prefix_and_key_constant() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    use pretty_assertions::assert_eq;

    let server = start_mock_server().await;
    let req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let TestCodex { codex, .. } = test_codex()
        .with_pre_build_hook(write_global_instructions)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CollaborationModes)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    // First turn
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 1".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    // Second turn using per-turn thread-settings overrides.
    let new_cwd = TempDir::new().unwrap();
    let writable = TempDir::new().unwrap();
    let permission_profile = PermissionProfile::workspace_write_with(
        &[writable.abs()],
        NetworkSandboxPolicy::Enabled,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    );
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(permission_profile, new_cwd.path());
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 2".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(new_cwd.abs())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                model: Some("o3".to_string()),
                effort: Some(Some(ReasoningEffort::High)),
                summary: Some(ReasoningSummary::Detailed),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request1 = req1.single_request();
    let request2 = req2.single_request();
    let body1 = request1.body_json();
    let body2 = request2.body_json();

    // prompt_cache_key should remain constant across per-turn overrides
    assert_eq!(
        body1["prompt_cache_key"], body2["prompt_cache_key"],
        "prompt_cache_key should not change across per-turn overrides"
    );

    let first_permissions = message_texts(&body1, "developer")
        .into_iter()
        .find(|text| text.starts_with("<permissions instructions>"))
        .expect("initial permissions context");
    let second_permissions = message_texts(&body2, "developer")
        .into_iter()
        .find(|text| text.starts_with("<permissions instructions>"))
        .expect("updated permissions context");
    assert_ne!(first_permissions, second_permissions);
    assert!(
        request2.has_message_with_input_texts("developer", |texts| {
            texts.iter().any(|text| text.contains("<model_switch>"))
        }),
        "expected model switch section after model override"
    );
    let env_contexts = environment_contexts(&body2);
    assert_eq!(env_contexts.len(), 1);
    let env_text = env_contexts[0];
    let expected_cwd = new_cwd.path().display().to_string();
    assert_default_env_context(env_text, &expected_cwd);
    let second_user_texts = message_texts(&body2, "user");
    assert!(second_user_texts.contains(&"hello 1"));
    assert!(second_user_texts.contains(&"hello 2"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_user_turn_with_no_changes_does_not_send_environment_context() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let TestCodex {
        codex,
        config,
        session_configured,
        ..
    } = test_codex()
        .with_pre_build_hook(write_global_instructions)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CollaborationModes)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    let default_cwd = config.cwd.clone();
    let default_approval_policy = config.permissions.approval_policy.value();
    let default_sandbox_policy = &config.legacy_sandbox_policy();
    let default_model = session_configured.model;
    let default_effort = config.model_reasoning_effort.clone();
    let default_summary = config.model_reasoning_summary;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 1".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(default_cwd.clone())),
                approval_policy: Some(default_approval_policy),
                sandbox_policy: Some(default_sandbox_policy.clone()),
                summary: Some(default_summary.unwrap_or(ReasoningSummary::Auto)),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: default_model.clone(),
                        reasoning_effort: default_effort.clone(),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 2".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(default_cwd.clone())),
                approval_policy: Some(default_approval_policy),
                sandbox_policy: Some(default_sandbox_policy.clone()),
                summary: Some(default_summary.unwrap_or(ReasoningSummary::Auto)),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: default_model.clone(),
                        reasoning_effort: default_effort,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request1 = req1.single_request();
    let request2 = req2.single_request();
    let body1 = request1.body_json();
    let body2 = request2.body_json();

    let default_cwd_lossy = default_cwd.to_string_lossy();
    let env_1 = environment_contexts(&body1);
    let env_2 = environment_contexts(&body2);
    assert_eq!(env_1.len(), 1);
    assert_eq!(env_2.len(), 1);
    assert_default_env_context(env_1[0], &default_cwd_lossy);
    assert_eq!(env_1, env_2);

    let input1 = body1["input"].as_array().expect("first input array");
    let input2 = body2["input"].as_array().expect("second input array");
    assert_eq_without_metadata(
        serde_json::Value::Array(input2[..input1.len()].to_vec()),
        serde_json::Value::Array(input1.clone()),
    );
    assert_eq_without_metadata(
        input2[input1.len()].clone(),
        text_user_input("hello 2".to_string()),
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_user_turn_with_changes_sends_environment_context() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));
    use pretty_assertions::assert_eq;

    let server = start_mock_server().await;

    let req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;
    let TestCodex {
        codex,
        config,
        session_configured,
        ..
    } = test_codex()
        .with_pre_build_hook(write_global_instructions)
        .with_config(|config| {
            config
                .features
                .enable(Feature::CollaborationModes)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    let default_cwd = config.cwd.clone();
    let default_approval_policy = config.permissions.approval_policy.value();
    let default_sandbox_policy = &config.legacy_sandbox_policy();
    let default_model = session_configured.model;
    let default_effort = config.model_reasoning_effort.clone();
    let default_summary = config.model_reasoning_summary;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 1".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(default_cwd.clone())),
                approval_policy: Some(default_approval_policy),
                sandbox_policy: Some(default_sandbox_policy.clone()),
                summary: Some(default_summary.unwrap_or(ReasoningSummary::Auto)),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: default_model,
                        reasoning_effort: default_effort,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, default_cwd.as_path());
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello 2".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(default_cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                summary: Some(ReasoningSummary::Detailed),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: "o3".to_string(),
                        reasoning_effort: Some(ReasoningEffort::High),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request1 = req1.single_request();
    let request2 = req2.single_request();
    let body1 = request1.body_json();
    let body2 = request2.body_json();

    let initial_env = environment_contexts(&body1);
    assert_eq!(initial_env.len(), 1);
    assert_default_env_context(initial_env[0], &default_cwd.to_string_lossy());
    assert!(
        request2.has_message_with_input_texts("developer", |texts| {
            texts.iter().any(|text| text.contains("<model_switch>"))
        }),
        "expected model switch section after model override"
    );
    let updated_env = environment_contexts(&body2);
    assert_eq!(updated_env.len(), 1);
    let expected_env_update_text = updated_env[0];
    assert_env_context_fragment(expected_env_update_text);
    assert!(
        expected_env_update_text.contains(
            "<permission_profile type=\"disabled\"><file_system type=\"unrestricted\" /></permission_profile>",
        ),
        "expected disabled filesystem profile in environment context: {expected_env_update_text}"
    );
    let second_user_texts = message_texts(&body2, "user");
    assert!(second_user_texts.contains(&"hello 1"));
    assert!(second_user_texts.contains(&"hello 2"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolved_reasoning_is_evicted_after_next_instruction_but_persisted_in_rollout()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let plan_args = serde_json::json!({
        "explanation": "reasoning projection integration test",
        "plan": [{"step": "complete tool follow-up", "status": "in_progress"}],
    })
    .to_string();
    let response_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("response-1"),
                ev_reasoning_item("rs_reasoning_1", &["summary one"], &["plaintext one"]),
                ev_function_call("plan-call", "update_plan", &plan_args),
                ev_completed("response-1"),
            ]),
            sse(vec![
                ev_response_created("response-2"),
                ev_reasoning_item("rs_reasoning_2", &["summary two"], &["plaintext two"]),
                ev_assistant_message("message-2", "tool follow-up complete"),
                ev_completed("response-2"),
            ]),
            sse(vec![
                ev_response_created("response-3"),
                ev_assistant_message("message-3", "new instruction complete"),
                ev_completed("response-3"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::ItemIds)
            .expect("test config should allow item IDs");
    });
    let test = builder.build(&server).await?;
    let rollout_path = test
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    test.submit_turn("first instruction").await?;
    test.submit_turn("second instruction").await?;

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].inputs_of_type("reasoning").is_empty());

    let request_2_reasoning = requests[1].inputs_of_type("reasoning");
    assert_eq!(request_2_reasoning.len(), 1);
    assert_eq!(request_2_reasoning[0]["id"], "rs_reasoning_1");
    assert_eq!(request_2_reasoning[0]["summary"][0]["text"], "summary one");
    assert_eq!(
        request_2_reasoning[0]["content"][0]["text"],
        "plaintext one"
    );
    assert!(request_2_reasoning[0]["encrypted_content"].is_string());
    assert!(
        requests[1]
            .inputs_of_type("function_call_output")
            .iter()
            .any(|item| item["call_id"] == "plan-call")
    );

    assert!(requests[2].inputs_of_type("reasoning").is_empty());

    test.codex.flush_rollout().await?;
    let persisted_reasoning = persisted_reasoning_items(&rollout_path);
    assert_eq!(persisted_reasoning.len(), 2);
    for (item, expected_summary, expected_plaintext) in [
        (&persisted_reasoning[0], "summary one", "plaintext one"),
        (&persisted_reasoning[1], "summary two", "plaintext two"),
    ] {
        let item = serde_json::to_value(item)?;
        assert_eq!(item["summary"][0]["text"], expected_summary);
        assert_eq!(item["content"][0]["text"], expected_plaintext);
        assert!(item["encrypted_content"].is_string());
    }

    Ok(())
}
