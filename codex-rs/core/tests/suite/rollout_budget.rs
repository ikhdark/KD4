use anyhow::Result;
use codex_core::config::RolloutBudgetConfig;
use codex_features::Feature;
use codex_features::RolloutBudgetAction;
use codex_model_provider_info::built_in_model_providers;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use test_case::test_case;
use tokio::time::timeout;

const MULTI_AGENT_V2_NAMESPACE: &str = "agents";

fn rollout_budget() -> RolloutBudgetConfig {
    RolloutBudgetConfig {
        limit_tokens: 100,
        reminder_at_remaining_tokens: vec![75, 50, 25],
        sampling_token_weight: 1.0,
        prefill_token_weight: 1.0,
        cached_input_token_weight: 0.0,
        model_call_token_cost: 0.0,
        tool_output_byte_weight: 0.0,
        subagent_token_cost: 0.0,
        action: RolloutBudgetAction::Stop,
    }
}

fn rollout_budget_texts(request: &ResponsesRequest) -> Vec<String> {
    request
        .message_input_texts("developer")
        .into_iter()
        .filter(|text| text.starts_with("<rollout_budget>"))
        .collect()
}

fn rollout_budget_message(remaining_tokens: i64) -> String {
    format!(
        "<rollout_budget>\nYou have {remaining_tokens} weighted tokens left in the shared session token budget.\n</rollout_budget>"
    )
}

fn wire_request_contains(request: &wiremock::Request, text: &str) -> bool {
    decoded_body(request)
        .and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn request_has_input_type(request: &wiremock::Request, input_type: &str) -> bool {
    decoded_body(request)
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .and_then(|body| {
            body.get("input")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some(input_type)
            })
        })
}

fn responses_request_has_input_type(request: &ResponsesRequest, input_type: &str) -> bool {
    request
        .body_json()
        .get("input")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(serde_json::Value::as_str) == Some(input_type)
            })
        })
}

fn decoded_body(request: &wiremock::Request) -> Option<Vec<u8>> {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&request.body)).ok()
    } else {
        Some(request.body.clone())
    }
}

fn request_input_types(request: &wiremock::Request) -> Vec<String> {
    decoded_body(request)
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .and_then(|body| {
            body.get("input")
                .and_then(serde_json::Value::as_array)
                .cloned()
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|item| {
            item.get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adds_weighted_initial_and_threshold_reminders() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp-1",
                        "usage": {
                            "input_tokens": 60,
                            "input_tokens_details": { "cached_tokens": 40 },
                            "output_tokens": 15,
                            "output_tokens_details": null,
                            "total_tokens": 75
                        }
                    }
                }),
            ]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rollout_budget = Some(RolloutBudgetConfig {
                sampling_token_weight: 2.0,
                prefill_token_weight: 0.5,
                ..rollout_budget()
            });
        })
        .build(&server)
        .await?;

    test.submit_turn("first turn").await?;
    test.submit_turn("second turn").await?;

    let requests = responses.requests();
    assert_eq!(
        rollout_budget_texts(&requests[0]),
        vec![rollout_budget_message(/*remaining_tokens*/ 100)]
    );
    assert_eq!(
        rollout_budget_texts(&requests[1]),
        vec![
            rollout_budget_message(/*remaining_tokens*/ 100),
            rollout_budget_message(/*remaining_tokens*/ 60),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_usage_draws_from_the_shared_budget() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const ROOT_PROMPT: &str = "spawn a budget worker";
    const CHILD_PROMPT: &str = "consume child budget";
    const FOLLOW_UP_PROMPT: &str = "report the shared budget";
    const SPAWN_CALL_ID: &str = "spawn-budget-worker";
    const CLASSIFY_PROMPT: &str = "classify this budget integration task";
    const CLASSIFY_CALL_ID: &str = "classify-budget-task";

    let server = start_mock_server().await;
    let isolated_cwd = tempfile::tempdir()?;
    let isolated_cwd_path = AbsolutePathBuf::try_from(isolated_cwd.path())?;
    let classify_args = json!({
        "operation": "classify",
        "exhaustive": false,
        "risk_domains": [],
        "supported_non_git_roots": [],
    })
    .to_string();
    let classify_start = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| wire_request_contains(request, CLASSIFY_PROMPT),
        sse(vec![
            ev_response_created("classify-1"),
            ev_function_call(CLASSIFY_CALL_ID, "task_state", &classify_args),
            ev_completed("classify-1"),
        ]),
    )
    .await;
    let classify_continue = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| wire_request_contains(request, CLASSIFY_CALL_ID),
        sse(vec![
            ev_response_created("classify-2"),
            ev_completed("classify-2"),
        ]),
    )
    .await;
    let spawn_args = json!({
        "fork_turns": "none",
        "message": CHILD_PROMPT,
        "task_name": "budget_worker",
    })
    .to_string();
    let root_spawn = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| wire_request_contains(request, ROOT_PROMPT),
        sse(vec![
            ev_response_created("root-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed_with_tokens("root-1", /*total_tokens*/ 10),
        ]),
    )
    .await;
    let child_run = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_has_input_type(request, "agent_message"),
        sse(vec![
            ev_response_created("child-1"),
            ev_assistant_message("child-message", "child budget consumed"),
            ev_completed_with_tokens("child-1", /*total_tokens*/ 30),
        ]),
    )
    .await;
    let root_continue = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            wire_request_contains(request, SPAWN_CALL_ID)
                && !request_has_input_type(request, "agent_message")
        },
        sse(vec![
            ev_response_created("root-2"),
            ev_completed_with_tokens("root-2", /*total_tokens*/ 10),
        ]),
    )
    .await;
    let follow_up = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| wire_request_contains(request, FOLLOW_UP_PROMPT),
        sse(vec![ev_response_created("root-3"), ev_completed("root-3")]),
    )
    .await;

    let test = test_codex()
        .with_config(move |config| {
            config.cwd = isolated_cwd_path;
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow multi-agent tools");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow multi-agent v2");
            config.rollout_budget = Some(rollout_budget());
        })
        .build(&server)
        .await?;

    test.submit_turn(CLASSIFY_PROMPT).await?;
    let mut created_threads = test.thread_manager.subscribe_thread_created();
    let root_result = timeout(Duration::from_secs(10), test.submit_turn(ROOT_PROMPT)).await;
    let Ok(root_result) = root_result else {
        anyhow::bail!(
            "root turn timed out: spawn={}, child={}, continuation={}",
            root_spawn.requests().len(),
            child_run.requests().len(),
            root_continue.requests().len()
        );
    };
    root_result?;
    let child_thread_id = match timeout(Duration::from_secs(10), created_threads.recv()).await {
        Ok(Ok(child_thread_id)) => child_thread_id,
        Ok(Err(error)) => return Err(error.into()),
        Err(_) => {
            let spawn_output = root_continue
                .requests()
                .into_iter()
                .find_map(|request| request.function_call_output_text(SPAWN_CALL_ID));
            anyhow::bail!(
                "child thread was not created: spawn={}, child={}, continuation={}, spawn_output={spawn_output:?}",
                root_spawn.requests().len(),
                child_run.requests().len(),
                root_continue.requests().len()
            );
        }
    };
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let child_events = std::sync::Mutex::new(Vec::<String>::new());
    let child_complete = timeout(Duration::from_secs(10), async {
        loop {
            let event = child_thread
                .next_event()
                .await
                .expect("child event stream ended before turn completion");
            let is_complete = matches!(event.msg, EventMsg::TurnComplete(_));
            let mut events = child_events.lock().expect("child events lock poisoned");
            events.push(format!("{:?}", event.msg));
            if events.len() > 8 {
                events.remove(0);
            }
            drop(events);
            if is_complete {
                break;
            }
        }
    })
    .await;
    if child_complete.is_err() {
        let request_shapes = server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|request| {
                (
                    request.url.path().to_string(),
                    request_input_types(&request),
                )
            })
            .collect::<Vec<_>>();
        anyhow::bail!(
            "child turn did not complete: spawn={}, child={}, continuation={}, events={:?}, requests={request_shapes:?}",
            root_spawn.requests().len(),
            child_run.requests().len(),
            root_continue.requests().len(),
            child_events
                .into_inner()
                .expect("child events lock poisoned")
        );
    }
    timeout(Duration::from_secs(10), test.submit_turn(FOLLOW_UP_PROMPT)).await??;

    let requests = follow_up
        .requests()
        .into_iter()
        .filter(|request| {
            request
                .message_input_texts("user")
                .iter()
                .any(|text| text == FOLLOW_UP_PROMPT)
        })
        .collect::<Vec<_>>();
    let [request] = requests.as_slice() else {
        anyhow::bail!("expected 1 follow-up request, got {}", requests.len());
    };
    assert_eq!(
        rollout_budget_texts(request).last(),
        Some(&rollout_budget_message(/*remaining_tokens*/ 50))
    );
    let child_sample_request_count = child_run
        .requests()
        .iter()
        .filter(|request| responses_request_has_input_type(request, "agent_message"))
        .count();
    assert_eq!(child_sample_request_count, 1);
    assert_eq!(classify_start.requests().len(), 1);
    assert_eq!(classify_continue.requests().len(), 1);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_budget_fails_current_and_later_turns_without_another_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("exhaust-budget"),
            ev_completed_with_tokens("exhaust-budget", /*total_tokens*/ 30),
        ])],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 30,
                reminder_at_remaining_tokens: vec![20, 10],
                ..rollout_budget()
            });
        })
        .build(&server)
        .await?;

    for prompt in ["exhaust the budget", "try another turn"] {
        test.codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: prompt.to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await?;

        wait_for_event(&test.codex, |event| {
            matches!(
                event,
                EventMsg::Error(error)
                    if error.codex_error_info == Some(CodexErrorInfo::SessionBudgetExceeded)
            )
        })
        .await;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    assert_eq!(
        responses.requests().len(),
        1,
        "known budget exhaustion should fail before another model request"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ask_budget_requires_exact_approval_before_another_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const APPROVAL: &str = "approve additional budget";
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("exhaust-ask-budget"),
                ev_completed_with_tokens("exhaust-ask-budget", /*total_tokens*/ 30),
            ]),
            sse(vec![
                ev_response_created("approved-budget"),
                ev_completed("approved-budget"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 30,
                reminder_at_remaining_tokens: vec![20, 10],
                action: RolloutBudgetAction::Ask,
                ..rollout_budget()
            });
        })
        .build(&server)
        .await?;

    for prompt in ["exhaust the ask budget", "this is not approval"] {
        test.codex
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: prompt.to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
                additional_context: Default::default(),
                thread_settings: Default::default(),
            })
            .await?;
        wait_for_event(&test.codex, |event| {
            matches!(
                event,
                EventMsg::Error(error)
                    if error.codex_error_info == Some(CodexErrorInfo::SessionBudgetExceeded)
            )
        })
        .await;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }

    assert_eq!(
        responses.requests().len(),
        1,
        "a different user turn must not implicitly approve more budget"
    );
    test.submit_turn(APPROVAL).await?;
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1]
            .message_input_texts("user")
            .iter()
            .any(|text| text == APPROVAL)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[test_case(false ; "local")]
#[test_case(true ; "remote_v2")]
async fn compaction_budget_exhaustion_fails_without_retry(remote_v2: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let compact_response = if remote_v2 {
        sse(vec![
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "compaction",
                    "encrypted_content": "encrypted-summary",
                }
            }),
            ev_completed_with_tokens("compact", /*total_tokens*/ 10),
        ])
    } else {
        sse(vec![
            ev_response_created("compact"),
            ev_assistant_message("compact-summary", "compact summary"),
            ev_completed_with_tokens("compact", /*total_tokens*/ 10),
        ])
    };
    let responses = mount_sse_sequence(&server, vec![compact_response]).await;
    let test = test_codex()
        .with_config(move |config| {
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 10,
                reminder_at_remaining_tokens: vec![5],
                ..rollout_budget()
            });
            if remote_v2 {
                config
                    .features
                    .enable(Feature::RemoteCompactionV2)
                    .expect("test config should allow remote compaction v2");
            } else {
                config.model_provider.name = "OpenAI-compatible test provider".to_string();
            }
        })
        .build(&server)
        .await?;

    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(
            event,
            EventMsg::Error(error)
                if error.codex_error_info == Some(CodexErrorInfo::SessionBudgetExceeded)
        )
    })
    .await;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(responses.requests().len(), 1, "compaction should not retry");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restates_the_current_remainder_after_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_completed_with_tokens("resp-1", /*total_tokens*/ 20),
            ]),
            sse(vec![
                ev_response_created("resp-compact"),
                ev_assistant_message("msg-compact", "compact summary"),
                ev_completed_with_tokens("resp-compact", /*total_tokens*/ 10),
            ]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;
    let mut model_provider = built_in_model_providers(/*openai_base_url*/ None)["openai"].clone();
    model_provider.name = "OpenAI-compatible test provider".to_string();
    model_provider.base_url = Some(format!("{}/v1", server.uri()));
    model_provider.supports_websockets = false;
    let test = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
            config.rollout_budget = Some(RolloutBudgetConfig {
                reminder_at_remaining_tokens: vec![50],
                ..rollout_budget()
            });
        })
        .build(&server)
        .await?;

    test.submit_turn("first turn").await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_turn("second turn").await?;

    let requests = responses.requests();
    assert_eq!(
        rollout_budget_texts(&requests[2]),
        vec![rollout_budget_message(/*remaining_tokens*/ 70)],
        "a new context window should restate the current remainder"
    );
    let request_body = requests[2].body_json().to_string();
    let summary_position = request_body
        .find("compact summary")
        .expect("post-compaction request should contain the summary");
    let reminder_position = request_body
        .find("You have 70 weighted tokens left in the shared session token budget.")
        .expect("post-compaction request should contain the current remainder");
    assert!(
        summary_position < reminder_position,
        "the current remainder should follow the compaction summary"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restates_the_current_remainder_after_rollback() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_completed_with_tokens("resp-1", /*total_tokens*/ 30),
            ]),
            sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.rollout_budget = Some(RolloutBudgetConfig {
                reminder_at_remaining_tokens: vec![50],
                ..rollout_budget()
            });
        })
        .build(&server)
        .await?;

    test.submit_turn("rolled-back turn").await?;
    test.codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ThreadRolledBack(_))
    })
    .await;
    test.submit_turn("turn after rollback").await?;

    let requests = responses.requests();
    assert_eq!(
        rollout_budget_texts(&requests[1]),
        vec![rollout_budget_message(/*remaining_tokens*/ 70)],
        "rollback should rearm the current budget reminder without refunding usage"
    );

    Ok(())
}
