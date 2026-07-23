use core_test_support::test_codex::local_selections;
use std::fs;
use std::sync::Arc;

use anyhow::Result;
use codex_config::types::Personality;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::PathBufExt;
use core_test_support::context_snapshot;
use core_test_support::context_snapshot::ContextSnapshotOptions;
use core_test_support::context_snapshot::ContextSnapshotRenderMode;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use serde_json::json;

const PRETURN_CONTEXT_DIFF_CWD: &str = "PRETURN_CONTEXT_DIFF_CWD";

fn context_snapshot_options() -> ContextSnapshotOptions {
    ContextSnapshotOptions::default()
        .render_mode(ContextSnapshotRenderMode::KindWithTextPrefix { max_chars: 96 })
}

fn format_labeled_requests_snapshot(
    scenario: &str,
    sections: &[(&str, &ResponsesRequest)],
) -> String {
    context_snapshot::format_labeled_requests_snapshot(
        scenario,
        sections,
        &context_snapshot_options(),
    )
}

fn format_environment_context_subagents_snapshot(subagents: &[&str]) -> String {
    let subagents_block = if subagents.is_empty() {
        String::new()
    } else {
        let lines = subagents
            .iter()
            .map(|line| format!("    {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n  <subagents>\n{lines}\n  </subagents>")
    };
    let items = vec![json!({
        "type": "message",
        "role": "user",
        "content": [{
            "type": "input_text",
            "text": format!(
                "<environment_context>\n  <cwd>/tmp/example</cwd>\n  <shell>bash</shell>{subagents_block}\n</environment_context>"
            ),
        }],
    })];
    context_snapshot::format_response_items_snapshot(items.as_slice(), &context_snapshot_options())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_model_visible_layout_turn_overrides() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "turn one complete"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "turn two complete"),
                ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex().with_model("gpt-5.4").with_config(|config| {
        config
            .features
            .enable(Feature::Personality)
            .expect("test config should allow feature update");
        config.personality = Some(Personality::Pragmatic);
    });
    let test = builder.build(&server).await?;
    let preturn_context_diff_cwd = test.cwd_path().join(PRETURN_CONTEXT_DIFF_CWD);
    fs::create_dir_all(&preturn_context_diff_cwd)?;
    let preturn_context_diff_cwd = preturn_context_diff_cwd.abs();
    let first_turn_cwd = test.config.cwd.clone();
    let (first_sandbox_policy, first_permission_profile) =
        turn_permission_fields(PermissionProfile::read_only(), first_turn_cwd.as_path());

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first turn".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(first_turn_cwd)),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(first_sandbox_policy),
                permission_profile: first_permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: test.config.model_reasoning_effort.clone(),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let (second_sandbox_policy, second_permission_profile) = turn_permission_fields(
        PermissionProfile::read_only(),
        preturn_context_diff_cwd.as_path(),
    );
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second turn with context updates".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(preturn_context_diff_cwd)),
                approval_policy: Some(AskForApproval::OnRequest),
                sandbox_policy: Some(second_sandbox_policy),
                permission_profile: second_permission_profile,
                personality: Some(Personality::Friendly),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: test.config.model_reasoning_effort.clone(),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2, "expected two requests");
    insta::assert_snapshot!(
        "model_visible_layout_turn_overrides",
        format_labeled_requests_snapshot(
            "Second turn changes cwd, approval policy, and personality while keeping model constant.",
            &[
                ("First Request (Baseline)", &requests[0]),
                ("Second Request (Turn Overrides)", &requests[1]),
            ]
        )
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_model_visible_layout_refreshes_agents_between_turns() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_assistant_message("msg-1", "turn one complete"),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_assistant_message("msg-2", "turn two complete"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_assistant_message("msg-3", "turn three complete"),
                ev_completed("resp-3"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex().with_model("gpt-5.4");
    let test = builder.build(&server).await?;
    let cwd_one = test.cwd_path().join("agents_one");
    let cwd_two = test.cwd_path().join("agents_two");
    fs::create_dir_all(&cwd_one)?;
    fs::create_dir_all(&cwd_two)?;
    fs::write(
        cwd_one.join("AGENTS.md"),
        "# AGENTS one\n\n<INSTRUCTIONS>\nTurn one agents instructions.\n</INSTRUCTIONS>\n",
    )?;
    let second_agents =
        "# AGENTS two\n\n<INSTRUCTIONS>\nTurn blue agents instructions.\n</INSTRUCTIONS>\n";
    let edited_agents =
        "# AGENTS two\n\n<INSTRUCTIONS>\nTurn gold agents instructions.\n</INSTRUCTIONS>\n";
    assert_eq!(second_agents.len(), edited_agents.len());
    fs::write(cwd_two.join("AGENTS.md"), second_agents)?;
    let cwd_one = cwd_one.abs();
    let cwd_two = cwd_two.abs();
    let (first_sandbox_policy, first_permission_profile) =
        turn_permission_fields(PermissionProfile::read_only(), cwd_one.as_path());

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first turn in agents_one".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(cwd_one.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(first_sandbox_policy),
                permission_profile: first_permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: test.config.model_reasoning_effort.clone(),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let (second_sandbox_policy, second_permission_profile) =
        turn_permission_fields(PermissionProfile::read_only(), cwd_two.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second turn in agents_two".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(cwd_two.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(second_sandbox_policy),
                permission_profile: second_permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: test.config.model_reasoning_effort.clone(),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    fs::write(cwd_two.join("AGENTS.md"), edited_agents)?;
    let (third_sandbox_policy, third_permission_profile) =
        turn_permission_fields(PermissionProfile::read_only(), cwd_two.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "third turn after same-cwd AGENTS edit".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(cwd_two)),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(third_sandbox_policy),
                permission_profile: third_permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: test.config.model_reasoning_effort.clone(),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3, "expected three requests");
    let first_agents = requests[0].message_input_texts("user");
    assert!(
        first_agents
            .iter()
            .any(|text| text.contains("Turn one agents instructions.")),
        "first request should include the first cwd's AGENTS.md instructions"
    );
    let second_agents = requests[1].message_input_texts("user");
    assert!(
        second_agents.iter().any(|text| {
            text.contains(
                "These AGENTS.md instructions replace all previously provided AGENTS.md instructions.",
            ) && text.contains("Turn blue agents instructions.")
        }),
        "second request should replace AGENTS.md after the cwd changes"
    );
    let edited_agents = requests[2].message_input_texts("user");
    assert!(
        edited_agents.iter().any(|text| {
            text.contains(
                "These AGENTS.md instructions replace all previously provided AGENTS.md instructions.",
            ) && text.contains("Turn gold agents instructions.")
        }),
        "third request should replace AGENTS.md after a same-cwd, same-length edit"
    );
    insta::assert_snapshot!(
        "model_visible_layout_refreshes_agents_between_turns",
        format_labeled_requests_snapshot(
            "Normal turns refresh AGENTS.md after a cwd change and after a same-cwd, same-length content edit.",
            &[
                ("First Request (agents_one)", &requests[0]),
                ("Second Request (agents_two cwd)", &requests[1]),
                ("Third Request (agents_two edited)", &requests[2]),
            ]
        )
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_model_visible_layout_resume_with_personality_change() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut initial_builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.2".to_string());
    });
    let initial = initial_builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    let initial_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-initial"),
            ev_assistant_message("msg-1", "recorded before resume"),
            ev_completed("resp-initial"),
        ]),
    )
    .await;
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "seed resume history".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let initial_request = initial_mock.single_request();

    let resumed_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-resume"),
            ev_assistant_message("msg-2", "first resumed turn"),
            ev_completed("resp-resume"),
        ]),
    )
    .await;

    let mut resume_builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.4".to_string());
        config
            .features
            .enable(Feature::Personality)
            .expect("test config should allow feature update");
        config.personality = Some(Personality::Pragmatic);
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    let resume_override_cwd = resumed.cwd_path().join(PRETURN_CONTEXT_DIFF_CWD);
    fs::create_dir_all(&resume_override_cwd)?;
    let resume_override_cwd = resume_override_cwd.abs();
    let (sandbox_policy, permission_profile) = turn_permission_fields(
        PermissionProfile::read_only(),
        resume_override_cwd.as_path(),
    );
    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "resume and change personality".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(resume_override_cwd)),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                personality: Some(Personality::Friendly),
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: resumed.session_configured.model.clone(),
                        reasoning_effort: resumed.config.model_reasoning_effort.clone(),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let resumed_request = resumed_mock.single_request();
    insta::assert_snapshot!(
        "model_visible_layout_resume_with_personality_change",
        format_labeled_requests_snapshot(
            "First post-resume turn where resumed config model differs from rollout and personality changes.",
            &[
                ("Last Request Before Resume", &initial_request),
                ("First Request After Resume", &resumed_request),
            ]
        )
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_model_visible_layout_resume_override_matches_rollout_model() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut initial_builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.2".to_string());
    });
    let initial = initial_builder.build(&server).await?;
    let codex = Arc::clone(&initial.codex);
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    let initial_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-initial"),
            ev_assistant_message("msg-1", "recorded before resume"),
            ev_completed("resp-initial"),
        ]),
    )
    .await;
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "seed resume history".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let initial_request = initial_mock.single_request();

    let resumed_mock = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-resume"),
            ev_assistant_message("msg-2", "first resumed turn"),
            ev_completed("resp-resume"),
        ]),
    )
    .await;

    let mut resume_builder = test_codex().with_config(|config| {
        config.model = Some("gpt-5.4".to_string());
    });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    let resume_override_cwd = resumed.cwd_path().join(PRETURN_CONTEXT_DIFF_CWD);
    fs::create_dir_all(&resume_override_cwd)?;
    let resume_override_cwd = resume_override_cwd.abs();
    core_test_support::submit_thread_settings(
        &resumed.codex,
        codex_protocol::protocol::ThreadSettingsOverrides {
            environments: Some(local_selections(resume_override_cwd)),
            model: Some("gpt-5.2".to_string()),
            ..Default::default()
        },
    )
    .await?;
    resumed
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first resumed turn after model override".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event(&resumed.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    let resumed_request = resumed_mock.single_request();
    insta::assert_snapshot!(
        "model_visible_layout_resume_override_matches_rollout_model",
        format_labeled_requests_snapshot(
            "First post-resume turn where pre-turn override sets model to rollout model; no model-switch update should appear.",
            &[
                ("Last Request Before Resume", &initial_request),
                ("First Request After Resume + Override", &resumed_request),
            ]
        )
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_model_visible_layout_environment_context_includes_one_subagent() -> Result<()> {
    insta::assert_snapshot!(
        "model_visible_layout_environment_context_includes_one_subagent",
        format_environment_context_subagents_snapshot(&["- agent-1: Atlas"])
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_model_visible_layout_environment_context_includes_two_subagents() -> Result<()> {
    insta::assert_snapshot!(
        "model_visible_layout_environment_context_includes_two_subagents",
        format_environment_context_subagents_snapshot(&["- agent-1: Atlas", "- agent-2: Juniper"])
    );

    Ok(())
}
