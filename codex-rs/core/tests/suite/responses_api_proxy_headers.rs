//! Verifies that parent and spawned subagent Responses API requests carry the expected window,
//! parent-thread, and subagent identity headers.

use anyhow::Result;
use anyhow::anyhow;
use codex_features::Feature;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::require_network;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;

const PARENT_PROMPT: &str = "spawn a subagent and report when it is started";
const CHILD_PROMPT: &str = "child: say done";
const SPAWN_CALL_ID: &str = "spawn-call-1";
const REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(/*millis*/ 20);
const TURN_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 60);
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_api_parent_and_subagent_requests_include_identity_headers() -> Result<()> {
    require_network!();

    let server = start_mock_server().await;

    let spawn_args = serde_json::to_string(&json!({ "message": CHILD_PROMPT }))?;
    let parent_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            request_body_contains(req, PARENT_PROMPT)
                && request_header(req, "x-openai-subagent").is_none()
        },
        sse(vec![
            ev_response_created("resp-parent-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                "multi_agent_v1",
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let child_mock = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            request_body_contains(req, CHILD_PROMPT)
                && request_header(req, "x-openai-subagent") == Some("collab_spawn")
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    // Child completion may arrive during the parent's continuation, requiring one more
    // request to consume the notification. Identity must hold in either ordering.
    let mut parent_follow_up_mocks = Vec::new();
    for index in 2..=3 {
        let response_id = format!("resp-parent-{index}");
        parent_follow_up_mocks.push(mount_sse_once_match(
            &server,
            |req: &wiremock::Request| {
                request_body_contains(req, SPAWN_CALL_ID)
                    && request_header(req, "x-openai-subagent").is_none()
            },
            sse(vec![
                ev_response_created(&response_id),
                ev_assistant_message(&format!("msg-parent-{index}"), "parent done"),
                ev_completed(&response_id),
            ]),
        ).await);
    }

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .disable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        config
            .features
            .disable(Feature::EnableRequestCompression)
            .expect("test config should allow feature update");
    });
    let test = builder.build(&server).await?;
    submit_turn_with_timeout(&test, PARENT_PROMPT, &server).await?;

    let parent = wait_for_matching_request(&parent_mock, "parent request", |request| {
        request.body_contains_text(PARENT_PROMPT) && request.header("x-openai-subagent").is_none()
    })
    .await?;
    let child = wait_for_matching_request(&child_mock, "child request", |request| {
        request.body_contains_text(CHILD_PROMPT)
            && request.header("x-openai-subagent").as_deref() == Some("collab_spawn")
    })
    .await?;

    let parent_window_id = parent
        .header("x-codex-window-id")
        .ok_or_else(|| anyhow!("parent request missing x-codex-window-id"))?;
    let child_window_id = child
        .header("x-codex-window-id")
        .ok_or_else(|| anyhow!("child request missing x-codex-window-id"))?;
    let (parent_thread_id, parent_generation) = split_window_id(&parent_window_id)?;
    let (child_thread_id, child_generation) = split_window_id(&child_window_id)?;
    assert_eq!(parent_thread_id, test.session_configured.thread_id.to_string());
    let continuations: Vec<_> = parent_follow_up_mocks.iter()
        .flat_map(ResponseMock::requests)
        .collect();
    assert!((1..=2).contains(&continuations.len()));
    for continuation in &continuations {
        let spawn_output = continuation.function_call_output_text(SPAWN_CALL_ID)
            .ok_or_else(|| anyhow!("parent continuation must retain its spawn result"))?;
        let spawn_result: serde_json::Value = serde_json::from_str(&spawn_output)?;
        assert_eq!(spawn_result["agent_id"].as_str(), Some(child_thread_id));
        assert_eq!(continuation.header("x-codex-window-id"), Some(parent_window_id.clone()));
        assert_eq!(continuation.header("x-codex-parent-thread-id"), None);
    }
    assert_eq!(parent.header("x-codex-parent-thread-id"), None);
    for request in [&parent, &child] {
        assert_eq!(
            request.body_json()["client_metadata"]["x-codex-window-id"].as_str(),
            request.header("x-codex-window-id").as_deref()
        );
    }

    assert_eq!(parent_generation, 0);
    assert_eq!(child_generation, 0);
    assert!(child_thread_id != parent_thread_id);
    assert_eq!(parent.header("x-openai-subagent"), None);
    assert_eq!(
        child.header("x-openai-subagent").as_deref(),
        Some("collab_spawn")
    );
    assert_eq!(
        child.header("x-codex-parent-thread-id").as_deref(),
        Some(parent_thread_id)
    );
    let child_turn_metadata: serde_json::Value = serde_json::from_str(
        &child
            .header("x-codex-turn-metadata")
            .ok_or_else(|| anyhow!("child request missing x-codex-turn-metadata"))?,
    )?;
    assert!(child_turn_metadata.get("forked_from_thread_id").is_none());
    assert_eq!(
        child_turn_metadata["parent_thread_id"].as_str(),
        Some(parent_thread_id)
    );

    Ok(())
}

async fn submit_turn_with_timeout(
    test: &TestCodex,
    prompt: &str,
    server: &wiremock::MockServer,
) -> Result<()> {
    let session_model = test.session_configured.model.clone();
    let cwd = test.config.cwd.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::workspace_write(), cwd.as_path());
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(cwd)),
                approval_policy: Some(AskForApproval::OnRequest),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                    mode: codex_protocol::config_types::ModeKind::Default,
                    settings: codex_protocol::config_types::Settings {
                        model: session_model,
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    let turn_started = wait_for_event_result(test, "turn started", |event| {
        matches!(event, EventMsg::TurnStarted(_))
    })
    .await?;
    let EventMsg::TurnStarted(turn_started) = turn_started else {
        unreachable!("event predicate only matches turn started events");
    };
    let completed = wait_for_event_result(test, "turn complete", |event| match event {
        EventMsg::TurnComplete(event) => event.turn_id == turn_started.turn_id,
        _ => false,
    })
    .await?;
    let EventMsg::TurnComplete(completed) = completed else {
        unreachable!("event predicate only accepts matching completion");
    };
    if completed.error.is_some() {
        eprintln!("diagnostic requests: {:#?}", server.received_requests().await);
    }
    assert_eq!(completed.error, None);

    Ok(())
}

async fn wait_for_matching_request<F>(
    mock: &ResponseMock,
    label: &str,
    mut predicate: F,
) -> Result<ResponsesRequest>
where
    F: FnMut(&ResponsesRequest) -> bool,
{
    tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            if let Some(request) = mock
                .requests()
                .into_iter()
                .find(|request| predicate(request))
            {
                return request;
            }
            tokio::time::sleep(REQUEST_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for {label}"))
}

async fn wait_for_event_result<F>(
    test: &TestCodex,
    stage: &str,
    mut predicate: F,
) -> Result<EventMsg>
where
    F: FnMut(&EventMsg) -> bool,
{
    let mut seen_events = Vec::new();
    tokio::time::timeout(TURN_TIMEOUT, async {
        loop {
            let event = test.codex.next_event().await?;
            seen_events.push(event_summary(&event.msg));
            if predicate(&event.msg) {
                return Ok::<EventMsg, anyhow::Error>(event.msg);
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow!(
            "timed out waiting for {stage}; saw events: {}",
            seen_events.join(" | ")
        )
    })?
}

fn event_summary(event: &EventMsg) -> String {
    let mut summary = format!("{event:?}");
    let mut end = summary.len().min(240);
    while !summary.is_char_boundary(end) {
        end -= 1;
    }
    summary.truncate(end);
    summary
}

#[test]
fn event_summary_preserves_utf8_within_the_diagnostic_byte_limit() {
    for message in ["short".to_string(), "x".repeat(300), "🦀".repeat(100)] {
        let event = EventMsg::Warning(codex_protocol::protocol::WarningEvent { message });
        let full = format!("{event:?}");
        let summary = event_summary(&event);
        assert!(summary.len() <= 240);
        assert!(full.starts_with(&summary));
        if full.len() <= 240 {
            assert_eq!(summary, full);
        } else {
            // Preserve the longest valid prefix within the byte budget, not
            // an empty placeholder or a lossy replacement character.
            let next = full[summary.len()..].chars().next().unwrap();
            assert!(summary.len() + next.len_utf8() > 240);
        }
    }
}

fn request_body_contains(req: &wiremock::Request, text: &str) -> bool {
    std::str::from_utf8(&req.body).is_ok_and(|body| body.contains(text))
}

fn request_header<'a>(req: &'a wiremock::Request, name: &str) -> Option<&'a str> {
    req.headers.get(name).and_then(|value| value.to_str().ok())
}

fn split_window_id(window_id: &str) -> Result<(&str, u64)> {
    let (thread_id, generation) = window_id
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("invalid window id header: {window_id}"))?;
    Ok((thread_id, generation.parse::<u64>()?))
}
