use anyhow::Result;
use codex_features::Feature;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::AdditionalContextKind;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::context_snapshot;
use core_test_support::context_snapshot::ContextSnapshotOptions;
use core_test_support::context_snapshot::ContextSnapshotRenderMode;
use core_test_support::require_network;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use indexmap::IndexMap;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_context_is_model_visible_but_not_a_user_message_item() -> Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.include_environment_context = false;
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "inspect the active tab".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([
                (
                    "browser_info".to_string(),
                    AdditionalContextEntry {
                        value: "tab one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
                (
                    "automation_info".to_string(),
                    AdditionalContextEntry {
                        value: "run one".to_string(),
                        kind: AdditionalContextKind::Application,
                    },
                ),
            ]),
            thread_settings: Default::default(),
        })
        .await?;

    let user_item = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::UserMessage(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;
    assert_eq!(
        user_item.content,
        vec![UserInput::Text {
            text: "inspect the active tab".to_string(),
            text_elements: Vec::new(),
        }]
    );
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    let request = request.single_request();
    insta::assert_snapshot!(
        "additional_context_simple_input",
        context_snapshot::format_labeled_requests_snapshot(
            "additional context is inserted before the user turn input.",
            &[("Request", &request)],
            &ContextSnapshotOptions::default()
                .strip_capability_instructions()
                .render_mode(ContextSnapshotRenderMode::KindWithTextPrefix { max_chars: 160 }),
        )
    );
    let developer_context_texts = request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.starts_with(application_context_prefix("automation_info").as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        developer_context_texts,
        vec![application_context("automation_info", "run one")]
    );
    assert_eq!(
        user_texts_without_task_model_guidance(&request),
        vec![
            external_context("browser_info", "tab one"),
            "inspect the active tab".to_string(),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_context_like_user_text_remains_a_user_message_item() -> Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.include_environment_context = false)
        .build(&server)
        .await?;
    let user_input = UserInput::Text {
        text: "<external_api>".to_string(),
        text_elements: Vec::new(),
    };

    test.codex
        .submit(Op::UserInput {
            items: vec![user_input.clone()],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::new(),
            thread_settings: Default::default(),
        })
        .await?;

    let user_item = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemCompleted(ItemCompletedEvent {
            item: TurnItem::UserMessage(item),
            ..
        }) => Some(item.clone()),
        _ => None,
    })
    .await;
    assert_eq!(user_item.content, vec![user_input]);
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    let request = request.single_request();
    assert_eq!(
        user_texts_without_task_model_guidance(&request),
        vec!["<external_api>"]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_context_trust_controls_message_role() -> Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.include_environment_context = false)
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "inspect context".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([
                (
                    "browser_info".to_string(),
                    AdditionalContextEntry {
                        value: "tab one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
                (
                    "automation_info".to_string(),
                    AdditionalContextEntry {
                        value: "run one".to_string(),
                        kind: AdditionalContextKind::Application,
                    },
                ),
            ]),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    let request = request.single_request();
    let developer_context_texts = request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.starts_with(application_context_prefix("automation_info").as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        developer_context_texts,
        vec![application_context("automation_info", "run one")]
    );
    assert_eq!(
        user_texts_without_task_model_guidance(&request),
        vec![
            external_context("browser_info", "tab one"),
            "inspect context".to_string(),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_context_is_deduplicated_between_turns_while_retained() -> Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let first_request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let second_request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.include_environment_context = false)
        .build(&server)
        .await?;
    let additional_context = IndexMap::from([(
        "browser_info".to_string(),
        AdditionalContextEntry {
            value: "same tab".to_string(),
            kind: AdditionalContextKind::Untrusted,
        },
    )]);

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: additional_context.clone(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context,
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    assert_eq!(
        user_texts_without_task_model_guidance(&first_request.single_request()),
        vec![
            external_context("browser_info", "same tab"),
            "first turn".to_string(),
        ]
    );
    assert_eq!(
        user_texts_without_task_model_guidance(&second_request.single_request()),
        vec![
            external_context("browser_info", "same tab"),
            "first turn".to_string(),
            "second turn".to_string(),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_context_removes_one_value_while_adding_another() -> Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let first_request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let second_request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;
    let third_request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-3"), ev_completed("resp-3")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.include_environment_context = false)
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "first turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([
                (
                    "automation_info".to_string(),
                    AdditionalContextEntry {
                        value: "run one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
                (
                    "browser_info".to_string(),
                    AdditionalContextEntry {
                        value: "tab one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
            ]),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "second turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([
                (
                    "automation_info".to_string(),
                    AdditionalContextEntry {
                        value: "run one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
                (
                    "terminal_info".to_string(),
                    AdditionalContextEntry {
                        value: "pty one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
            ]),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "third turn".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([
                (
                    "automation_info".to_string(),
                    AdditionalContextEntry {
                        value: "run one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
                (
                    "browser_info".to_string(),
                    AdditionalContextEntry {
                        value: "tab one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
                (
                    "terminal_info".to_string(),
                    AdditionalContextEntry {
                        value: "pty one".to_string(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
            ]),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    let reset = external_context(
        "__codex_additional_context_reset__",
        "Additional context snapshot replaced. All previously supplied additional context values are obsolete (previous_value_obsolete=\"true\"). Only additional-context entries following this reset in the current update remain available. Do not infer omitted values from earlier messages.",
    );
    assert_eq!(
        user_texts_without_task_model_guidance(&first_request.single_request()),
        vec![
            external_context("automation_info", "run one"),
            external_context("browser_info", "tab one"),
            "first turn".to_string(),
        ]
    );
    assert_eq!(
        user_texts_without_task_model_guidance(&second_request.single_request()),
        vec![
            external_context("automation_info", "run one"),
            external_context("browser_info", "tab one"),
            "first turn".to_string(),
            reset.clone(),
            external_context("automation_info", "run one"),
            external_context("terminal_info", "pty one"),
            "second turn".to_string(),
        ]
    );
    assert_eq!(
        user_texts_without_task_model_guidance(&third_request.single_request()),
        vec![
            external_context("automation_info", "run one"),
            external_context("browser_info", "tab one"),
            "first turn".to_string(),
            reset,
            external_context("automation_info", "run one"),
            external_context("terminal_info", "pty one"),
            "second turn".to_string(),
            external_context("browser_info", "tab one"),
            "third turn".to_string(),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_context_values_are_truncated_before_model_input() -> Result<()> {
    require_network!();

    const MAX_EXPECTED_EXTERNAL_CONTEXT_TEXT_BYTES: usize = 5 * 1024;

    let server = start_mock_server().await;
    let request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.include_environment_context = false)
        .build(&server)
        .await?;
    let long_browser_value = format!("browser-head-{}browser-tail", "b".repeat(40_000));
    let long_automation_value = format!("automation-head-{}automation-tail", "a".repeat(40_000));
    let untruncated_browser_fragment = external_context("browser_info", &long_browser_value);
    let untruncated_automation_fragment =
        application_context("automation_info", &long_automation_value);

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "summarize context".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::from([
                (
                    "automation_info".to_string(),
                    AdditionalContextEntry {
                        value: long_automation_value.clone(),
                        kind: AdditionalContextKind::Application,
                    },
                ),
                (
                    "browser_info".to_string(),
                    AdditionalContextEntry {
                        value: long_browser_value.clone(),
                        kind: AdditionalContextKind::Untrusted,
                    },
                ),
            ]),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;

    let request = request.single_request();
    let developer_texts = request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.starts_with(application_context_prefix("automation_info").as_str()))
        .collect::<Vec<_>>();
    let [automation_text] = developer_texts.as_slice() else {
        panic!("expected application additional context, got {developer_texts:?}");
    };
    assert!(automation_text.starts_with(&format!(
        "{}\nautomation-head-{}",
        application_context_prefix("automation_info"),
        "a".repeat(1024)
    )));
    assert!(automation_text.contains("tokens truncated"));
    assert!(automation_text.ends_with("automation-tail\n</application_context>"));
    assert!(automation_text.len() < untruncated_automation_fragment.len());
    assert!(
        automation_text.len() <= MAX_EXPECTED_EXTERNAL_CONTEXT_TEXT_BYTES,
        "application additional context was not capped before model input: {} bytes",
        automation_text.len()
    );

    let user_texts = user_texts_without_task_model_guidance(&request);
    let [external_text, user_text] = user_texts.as_slice() else {
        panic!("expected external context plus user input, got {user_texts:?}");
    };
    assert_eq!(user_text, "summarize context");
    assert!(external_text.starts_with(&format!(
        "{}\nbrowser-head-{}",
        external_context_prefix("browser_info"),
        "b".repeat(1024)
    )));
    assert!(external_text.contains("tokens truncated"));
    assert!(external_text.ends_with("browser-tail\n</external_context>"));
    assert!(external_text.len() < untruncated_browser_fragment.len());
    assert!(
        external_text.len() <= MAX_EXPECTED_EXTERNAL_CONTEXT_TEXT_BYTES,
        "untrusted additional context was not capped before model input: {} bytes",
        external_text.len()
    );

    Ok(())
}

fn external_context(source: &str, value: &str) -> String {
    format!(
        "{}\n{}\n</external_context>",
        external_context_prefix(source),
        value
    )
}

fn external_context_prefix(source: &str) -> String {
    format!("<external_context source=\"{source}\" kind=\"untrusted\">")
}

fn application_context(source: &str, value: &str) -> String {
    format!(
        "{}\n{}\n</application_context>",
        application_context_prefix(source),
        value
    )
}

fn application_context_prefix(source: &str) -> String {
    format!("<application_context source=\"{source}\" kind=\"application\">")
}

fn user_texts_without_task_model_guidance(request: &responses::ResponsesRequest) -> Vec<String> {
    request
        .message_input_texts("user")
        .into_iter()
        .filter(|text| !text.starts_with("<task_model_guidance>"))
        .collect()
}

fn task_model_guidance_texts(request: &responses::ResponsesRequest) -> Vec<String> {
    request
        .message_input_texts("user")
        .into_iter()
        .filter(|text| text.starts_with("<task_model_guidance>"))
        .collect()
}

async fn submit_plain_user_text(
    test: &core_test_support::test_codex::TestCodex,
    text: &str,
) -> Result<()> {
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: IndexMap::new(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_model_guidance_is_injected_only_when_the_feature_is_enabled() -> Result<()> {
    require_network!();

    for (instructions, base_owns_shared_policy) in [
        (codex_protocol::models::BASE_INSTRUCTIONS_DEFAULT.trim(), true),
        ("catalog supplied instructions", false),
    ] {
        for enabled in [false, true] {
            let server = start_mock_server().await;
            let test = test_codex()
                .with_config(move |config| {
                    config.include_environment_context = false;
                    config.base_instructions = Some(instructions.to_string());
                    config
                        .features
                        .set_enabled(Feature::TaskModelGuidance, enabled)
                        .expect("test config should allow feature update");
                })
                .build(&server)
                .await?;
            let mut first_guidance = None;
            for turn in 0..2 {
                let request = mount_sse_once(
                    &server,
                    sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
                )
                .await;
                submit_plain_user_text(&test, &format!("summarize guidance, turn {turn}")).await?;
                let request = request.single_request();
                assert_eq!(request.body_json()["instructions"], instructions);
                let guidance = task_model_guidance_texts(&request);
                assert_eq!(guidance.len(), usize::from(enabled));
                if enabled {
                    for required in [
                        "direct_file_read",
                        "Form competing hypotheses only when uncertainty between explanations affects the next action.",
                        "Track repository ownership and runtime relationships only as needed to establish the requested behavior.",
                        "These are internal evidence labels, not a mandatory user-facing reporting format.",
                    ] {
                        assert!(guidance[0].contains(required), "missing guidance: {required}");
                    }
                    let shared = "A no-change result is valid and preferred when the requested capability already exists adequately.";
                    assert_eq!(guidance[0].contains(shared), !base_owns_shared_policy);
                    assert_eq!(
                        instructions.matches(shared).count() + guidance[0].matches(shared).count(),
                        1
                    );
                    assert!(!guidance[0].contains("one to three plausible hypotheses"));
                    assert!(!guidance[0].contains("stay at module-level abstraction"));
                    assert!(guidance[0].ends_with("</task_model_guidance>"));
                }
                if let Some(first_guidance) = &first_guidance {
                    assert_eq!(
                        &guidance, first_guidance,
                        "second turn must not duplicate guidance"
                    );
                } else {
                    first_guidance = Some(guidance);
                }
            }
        }
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_model_guidance_owned_by_base_instructions_is_not_duplicated() -> Result<()> {
    require_network!();
    let server = start_mock_server().await;
    let request = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let instructions = "<task_model_guidance_policy version=\"1\" />\nRetain exact observed paths.";
    let test = test_codex()
        .with_config(move |config| {
            config.base_instructions = Some(instructions.to_string());
            config.features.enable(Feature::TaskModelGuidance).unwrap();
        })
        .build(&server)
        .await?;
    submit_plain_user_text(&test, "summarize the guidance policy").await?;
    let request = request.single_request();
    assert_eq!(request.body_json()["instructions"], instructions);
    assert!(task_model_guidance_texts(&request).is_empty());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_model_guidance_production_capture_matches_effective_settings() -> Result<()> {
    require_network!();

    // Isolate the opt-in trace environment from concurrently running tests.
    const TRACE_CHILD: &str = "CODEX_GUIDANCE_CAPTURE_TEST_CHILD";
    if std::env::var_os(TRACE_CHILD).is_none() {
        let trace_root = tempfile::tempdir()?;
        let output = tokio::process::Command::new(std::env::current_exe()?)
            .arg("--exact")
            .arg("suite::additional_context::task_model_guidance_production_capture_matches_effective_settings")
            .arg("--nocapture")
            .env(TRACE_CHILD, "1")
            .env(codex_rollout_trace::CODEX_ROLLOUT_TRACE_ROOT_ENV, trace_root.path())
            .kill_on_drop(true)
            .output()
            .await?;
        assert!(
            output.status.success(),
            "trace child failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "trace child must execute its assertions"
        );
        return Ok(());
    }

    let trace_root = std::path::PathBuf::from(
        std::env::var_os(codex_rollout_trace::CODEX_ROLLOUT_TRACE_ROOT_ENV)
            .expect("trace child has an explicit capture directory"),
    );
    let mut sampling_ids = std::collections::BTreeSet::new();
    let mut attempt_ids = std::collections::BTreeSet::new();
    for enabled in [false, true] {
        for owned in [false, true] {
            let instructions = if owned {
                "<task_model_guidance_policy version=\"1\" />\nRetain exact observed paths."
            } else {
                // Similar prose without the exact ownership marker must not
                // suppress the separate fragment when the feature is enabled.
                "Retain exact observed paths."
            };
            let server = start_mock_server().await;
            let test = test_codex()
                .with_config(move |config| {
                    config.include_environment_context = false;
                    config.base_instructions = Some(instructions.to_string());
                    config
                        .features
                        .set_enabled(Feature::TaskModelGuidance, enabled)
                        .unwrap();
                })
                .build(&server)
                .await?;
            let mut wire_requests = Vec::new();
            for turn in 0..2 {
                let request = mount_sse_once(
                    &server,
                    sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
                )
                .await;
                submit_plain_user_text(
                    &test,
                    &format!("capture enabled={enabled} owned={owned} turn={turn}"),
                )
                .await?;
                let request = request.single_request();
                let wire = request.body_json();
                assert!(wire.get("_codex").is_none());
                assert_eq!(wire["instructions"], instructions);
                assert!(!wire["tools"].as_array().expect("tool schemas").is_empty());
                assert_eq!(
                    task_model_guidance_texts(&request).len(),
                    usize::from(enabled && !owned)
                );
                wire_requests.push(wire);
            }
            // Stop trace producers before reading payload files.
            test.codex.shutdown_and_wait().await?;

            for wire in wire_requests {
                let mut captures = Vec::new();
                for bundle in std::fs::read_dir(&trace_root)? {
                    let payloads = bundle?.path().join("payloads");
                    for entry in std::fs::read_dir(payloads)? {
                        let value: serde_json::Value =
                            serde_json::from_slice(&std::fs::read(entry?.path())?)?;
                        if value.get("_codex").is_some() && value["input"] == wire["input"] {
                            captures.push(value);
                        }
                    }
                }
                assert_eq!(
                    captures.len(),
                    1,
                    "normal turn must save its production request"
                );
                let mut captured = captures.pop().unwrap();
                let metadata = captured.as_object_mut().unwrap().remove("_codex").unwrap();
                assert_eq!(
                    captured, wire,
                    "capture must preserve instructions, input, tools, and wire settings"
                );
                let settings = &metadata["settings"];
                assert_eq!(settings["task_model_guidance_enabled"], enabled);
                assert_eq!(settings["base_instructions_own_task_model_guidance"], owned);
                assert_eq!(
                    settings["enabled_features"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|key| key == Feature::TaskModelGuidance.key()),
                    enabled
                );
                assert!(settings.get("configured_reasoning_effort").is_some());
                assert!(settings.get("resolved_reasoning_effort").is_some());
                assert_eq!(
                    settings["resolved_reasoning_effort"],
                    wire["reasoning"]["effort"]
                );
                assert_eq!(settings["parallel_tool_calls"], wire["parallel_tool_calls"]);
                sampling_ids.insert(
                    metadata["sampling_request_id"]
                        .as_str()
                        .expect("sampling ID")
                        .to_string(),
                );
                attempt_ids.insert(
                    metadata["physical_attempt_id"]
                        .as_str()
                        .expect("attempt ID")
                        .to_string(),
                );
            }
        }
    }
    assert_eq!(
        sampling_ids.len(),
        8,
        "each turn has a distinct logical request"
    );
    assert_eq!(attempt_ids.len(), 8, "each dispatch has a distinct attempt");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_context_overflow_resets_stale_values_in_the_model_request() -> Result<()> {
    require_network!();

    let server = start_mock_server().await;
    let first = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let second = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;
    let third = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-3"), ev_completed("resp-3")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| config.include_environment_context = false)
        .build(&server)
        .await?;
    let unchanged = AdditionalContextEntry {
        value: "unchanged context remains available".to_string(),
        kind: AdditionalContextKind::Application,
    };
    let initial = IndexMap::from([
        ("unchanged".to_string(), unchanged.clone()),
        (
            "target".to_string(),
            AdditionalContextEntry {
                value: "old developer value".to_string(),
                kind: AdditionalContextKind::Application,
            },
        ),
    ]);
    let mut updated = IndexMap::from([("unchanged".to_string(), unchanged)]);
    for index in 0..80 {
        updated.insert(
            format!("long-source-{index:02}-{}", "\"\n\\".repeat(8_000)),
            AdditionalContextEntry {
                value: "a".repeat(20_000),
                kind: AdditionalContextKind::Application,
            },
        );
    }
    updated.insert(
        "target".to_string(),
        AdditionalContextEntry {
            value: "new untrusted value".to_string(),
            kind: AdditionalContextKind::Untrusted,
        },
    );

    for (text, additional_context) in [
        ("first turn", initial),
        ("second turn", updated.clone()),
        ("third turn", updated),
    ] {
        test.codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: text.to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context,
                thread_settings: Default::default(),
            })
            .await?;
        wait_for_event_match(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_)).then_some(())
        })
        .await;
    }

    let first_request = first.single_request();
    assert!(
        first_request
            .message_input_texts("developer")
            .contains(&application_context("target", "old developer value"))
    );

    // Keep model-request order and roles. The mock replaces only the external
    // model; Op::UserInput, context merging, history and request assembly are real.
    fn context_messages(request: &responses::ResponsesRequest) -> Vec<(String, String)> {
        request.body_json()["input"]
            .as_array()
            .expect("model input array")
            .iter()
            .filter_map(|item| {
                let role = item["role"].as_str()?;
                let content = item["content"].as_array()?;
                let [content] = content.as_slice() else {
                    return None;
                };
                let text = content["text"].as_str()?;
                (text.starts_with("<application_context source=")
                    || text.starts_with("<external_context source="))
                .then(|| (role.to_string(), text.to_string()))
            })
            .collect()
    }
    let second_messages = context_messages(&second.single_request());
    let reset_index = second_messages
        .iter()
        .position(|(_, text)| text.contains("__codex_additional_context_reset__"))
        .expect("overflow reset must reach the model");
    let current = &second_messages[reset_index..];
    assert_eq!(current[0].0, "developer");
    assert!(
        current[0]
            .1
            .contains("All previously supplied additional context values are obsolete")
    );
    assert!(current[0].1.contains("previous_value_obsolete=\"true\""));
    assert!(
        current[0]
            .1
            .contains("Only additional-context entries following this reset")
    );
    assert!(current[0].1.len() < 1_024);
    assert!(current.len() <= 256);
    assert!(current.iter().map(|(_, text)| text.len()).sum::<usize>() <= 160_000);
    assert!(current.iter().skip(1).any(|(role, text)| {
        role == "developer"
            && text == &application_context("unchanged", "unchanged context remains available")
    }));
    assert!(
        current
            .iter()
            .all(|(_, text)| !text.contains("old developer value"))
    );
    assert_eq!(
        second_messages
            .iter()
            .filter(|(_, text)| text.contains("__codex_additional_context_reset__"))
            .count(),
        1
    );
    assert_eq!(context_messages(&third.single_request()), second_messages);

    Ok(())
}
