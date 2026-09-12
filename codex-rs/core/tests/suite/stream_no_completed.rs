//! Verifies that the agent retries when the SSE stream terminates before
//! delivering a `response.completed` event.

use std::time::Duration;

use codex_app_server_protocol::ThreadHistoryBuilder;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::TurnStatus;
use codex_features::Feature;

use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnAbortReason;
use core_test_support::wait_for_event_with_timeout;
use tokio::sync::oneshot;

use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::WireApi;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;

fn sse_incomplete() -> String {
    responses::sse(vec![serde_json::json!({
        "type": "response.output_item.done",
    })])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_on_early_close() {
    skip_if_no_network!();

    let incomplete_sse = sse_incomplete();
    let completed_sse = responses::sse_completed("resp_ok");

    let (server, _) = start_streaming_sse_server(vec![
        vec![StreamingSseChunk {
            gate: None,
            body: incomplete_sse,
        }],
        vec![StreamingSseChunk {
            gate: None,
            body: completed_sse,
        }],
    ])
    .await;

    // Configure retry behavior explicitly to avoid mutating process-wide
    // environment variables.

    let model_provider = ModelProviderInfo {
        name: "openai".into(),
        base_url: Some(format!("{}/v1", server.uri())),
        // Environment variable that should exist in the test environment.
        // ModelClient will return an error if the environment variable for the
        // provider is not set.
        env_key: Some("PATH".into()),
        env_key_instructions: None,
        experimental_bearer_token: None,
        auth: None,
        aws: None,
        wire_api: WireApi::Responses,
        query_params: None,
        http_headers: None,
        env_http_headers: None,
        // exercise retry path: first attempt yields incomplete stream, so allow 1 retry
        request_max_retries: Some(0),
        stream_max_retries: Some(1),
        stream_idle_timeout_ms: Some(2000),
        websocket_connect_timeout_ms: None,
        requires_openai_auth: false,
        supports_websockets: false,
        supports_standalone_web_search: false,
    };

    let TestCodex { codex, .. } = test_codex()
        .with_config(move |config| {
            config.model_provider = model_provider;
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap();

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
        .unwrap();

    let EventMsg::StreamError(stream_error) =
        wait_for_event(&codex, |event| matches!(event, EventMsg::StreamError(_))).await
    else {
        unreachable!("predicate guarantees a stream error event");
    };
    assert_eq!(stream_error.message, "Reconnecting... 1/1");
    assert_eq!(
        stream_error.additional_details.as_deref(),
        Some("Error while reading the server response: stream closed before response.completed")
    );
    assert!(matches!(
        stream_error.codex_error_info,
        Some(CodexErrorInfo::ResponseStreamDisconnected {
            http_status_code: None
        })
    ));

    // Wait until TurnComplete (should succeed after the classified retry).
    wait_for_event(&codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;

    let requests = server.requests().await;
    assert_eq!(
        requests.len(),
        2,
        "expected retry after incomplete SSE stream"
    );

    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_close_completes_partial_assistant_before_terminal_error() {
    skip_if_no_network!();

    let (server, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: None,
        body: responses::sse(vec![
            responses::ev_response_created("resp_partial"),
            responses::ev_message_item_added("assistant-partial", ""),
            responses::ev_output_text_delta("The partial answer"),
        ]),
    }]])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap();
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Answer briefly".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
        .unwrap();

    let mut events = Vec::new();
    let terminal = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            events.push(event.clone());
            matches!(event, EventMsg::TurnComplete(_))
        },
        Duration::from_secs(10),
    )
    .await;
    let EventMsg::TurnComplete(terminal) = terminal else {
        unreachable!("predicate requires terminal completion");
    };
    let errors: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            EventMsg::Error(error) => Some((index, error)),
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 1, "the incomplete response must fail once");
    let (error_index, error) = errors[0];
    assert!(matches!(
        error.codex_error_info,
        Some(CodexErrorInfo::ResponseStreamConnectionFailed {
            http_status_code: None
        })
    ));
    assert_eq!(
        error.message,
        "Error while reading the server response: stream closed before response.completed"
    );
    assert_eq!(terminal.error.as_ref(), Some(error));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EventMsg::StreamError(_) | EventMsg::TurnAborted(_)))
    );

    let starts: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            EventMsg::ItemStarted(start) => match &start.item {
                TurnItem::AgentMessage(item) => Some((index, start, item)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let completions: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            EventMsg::ItemCompleted(completed) => match &completed.item {
                TurnItem::AgentMessage(item) => Some((index, completed, item)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(completions.len(), 1);
    let (start_index, start, started_item) = starts[0];
    let (completed_index, completed, completed_item) = completions[0];
    assert_eq!(started_item.id, "assistant-partial");
    assert_eq!(completed_item.id, started_item.id);
    assert_eq!(start.turn_id, terminal.turn_id);
    assert_eq!(completed.turn_id, terminal.turn_id);
    assert!(start_index < completed_index && completed_index < error_index);
    assert!(matches!(
        completed_item.content.as_slice(),
        [AgentMessageContent::Text { text }] if text == "The partial answer"
    ));
    let mut history = ThreadHistoryBuilder::new();
    for event in &events {
        history.handle_event(event);
    }
    assert_eq!(history.in_progress_turn_id(), None);
    let turns = history.finish();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].id, terminal.turn_id);
    assert_eq!(turns[0].status, TurnStatus::Failed);
    assert_eq!(
        turns[0].error.as_ref().map(|error| &error.message),
        Some(&error.message)
    );
    let messages: Vec<_> = turns[0]
        .items
        .iter()
        .filter_map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec!["The partial answer"]);
    assert_eq!(server.requests().await.len(), 1, "retries are disabled");
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_completes_partial_assistant_and_plan_before_turn_aborted() {
    skip_if_no_network!();

    let (release_stream, hold_stream) = oneshot::channel();
    let (server, _) = start_streaming_sse_server(vec![vec![
        StreamingSseChunk {
            gate: None,
            body: responses::sse(vec![
                responses::ev_response_created("resp_plan_partial"),
                responses::ev_message_item_added("assistant-plan-partial", ""),
                responses::ev_output_text_delta("Working on the plan.\n"),
                responses::ev_output_text_delta("<proposed_plan>\n1. Inspect the stream\n"),
            ]),
        },
        StreamingSseChunk {
            gate: Some(hold_stream),
            body: responses::sse_completed("resp_plan_partial"),
        },
    ]])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap();
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Plan the investigation".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Plan,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let mut events = Vec::new();
    let mut plan_deltas = String::new();
    wait_for_event_with_timeout(
        &test.codex,
        |event| {
            events.push(event.clone());
            if let EventMsg::PlanDelta(delta) = event {
                plan_deltas.push_str(&delta.delta);
            }
            plan_deltas == "1. Inspect the stream\n"
        },
        Duration::from_secs(10),
    )
    .await;
    // The provider is still gated. No output_item.done or response.completed
    // has arrived, so interrupt must close the two active items itself.
    assert!(!events.iter().any(|event| matches!(
        event,
        EventMsg::ItemCompleted(completed)
            if matches!(completed.item, TurnItem::AgentMessage(_) | TurnItem::Plan(_))
    )));
    let interrupt_index = events.len();
    test.codex.submit(Op::Interrupt).await.unwrap();
    let terminal = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            events.push(event.clone());
            matches!(event, EventMsg::TurnAborted(_))
        },
        Duration::from_secs(10),
    )
    .await;
    let EventMsg::TurnAborted(terminal) = terminal else {
        unreachable!("predicate requires turn abortion");
    };
    assert_eq!(terminal.reason, TurnAbortReason::Interrupted);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EventMsg::Error(_) | EventMsg::TurnComplete(_)))
    );
    let terminal_index = events.len() - 1;
    let starts: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            EventMsg::ItemStarted(start)
                if matches!(start.item, TurnItem::AgentMessage(_) | TurnItem::Plan(_)) =>
            {
                Some((index, start))
            }
            _ => None,
        })
        .collect();
    let completions: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            EventMsg::ItemCompleted(completed)
                if matches!(
                    completed.item,
                    TurnItem::AgentMessage(_) | TurnItem::Plan(_)
                ) =>
            {
                Some((index, completed))
            }
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 2, "assistant and plan must each start once");
    assert_eq!(
        starts
            .iter()
            .filter(|(_, start)| matches!(start.item, TurnItem::AgentMessage(_)))
            .count(),
        1,
        "exactly one assistant and one plan must start"
    );
    assert_eq!(
        completions.len(),
        2,
        "assistant and plan must each close once"
    );
    for (start_index, start) in starts {
        assert!(start_index < interrupt_index);
        assert_eq!(Some(&start.turn_id), terminal.turn_id.as_ref());
        let matching: Vec<_> = completions
            .iter()
            .filter(|(_, completed)| match (&start.item, &completed.item) {
                (TurnItem::AgentMessage(started), TurnItem::AgentMessage(finished)) => {
                    started.id == finished.id
                }
                (TurnItem::Plan(started), TurnItem::Plan(finished)) => started.id == finished.id,
                _ => false,
            })
            .collect();
        assert_eq!(matching.len(), 1, "each started ID must close exactly once");
        let (completed_index, completed) = *matching[0];
        assert!(interrupt_index <= completed_index && completed_index < terminal_index);
        assert_eq!(completed.turn_id, start.turn_id);
        match &completed.item {
            TurnItem::AgentMessage(item) => {
                assert_eq!(item.id, "assistant-plan-partial");
                assert!(matches!(
                    item.content.as_slice(),
                    [AgentMessageContent::Text { text }] if text == "Working on the plan.\n"
                ));
            }
            TurnItem::Plan(item) => {
                assert_eq!(item.id, format!("{}-plan", start.turn_id));
                assert_eq!(item.text, "1. Inspect the stream\n");
            }
            _ => unreachable!("only assistant and plan completions were selected"),
        }
    }
    assert_eq!(server.requests().await.len(), 1);
    let mut history = ThreadHistoryBuilder::new();
    for event in &events {
        history.handle_event(event);
    }
    assert_eq!(history.in_progress_turn_id(), None);
    let turns = history.finish();
    assert_eq!(turns.len(), 1);
    assert_eq!(Some(&turns[0].id), terminal.turn_id.as_ref());
    assert_eq!(turns[0].status, TurnStatus::Interrupted);
    assert_eq!(turns[0].error, None);
    let messages: Vec<_> = turns[0]
        .items
        .iter()
        .filter_map(|item| match item {
            ThreadItem::AgentMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, vec!["Working on the plan.\n"]);
    let plans: Vec<_> = turns[0]
        .items
        .iter()
        .filter_map(|item| match item {
            ThreadItem::Plan { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(plans, vec!["1. Inspect the stream\n"]);
    // Keep the stream open until after the interrupted terminal event; EOF
    // must not be what makes this test's item completion assertions pass.
    drop(release_stream);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plan_mode_empty_final_item_preserves_streamed_prose() {
    skip_if_no_network!();

    let (server, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: None,
        body: responses::sse(vec![
            responses::ev_response_created("resp_empty_final"),
            responses::ev_message_item_added("assistant-empty-final", ""),
            responses::ev_output_text_delta("I will inspect the stream.\n"),
            responses::ev_assistant_message("assistant-empty-final", ""),
            responses::ev_completed("resp_empty_final"),
        ]),
    }]])
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_streaming_server(&server)
        .await
        .unwrap();
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Explain the first investigation step".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Plan,
                    settings: Settings {
                        model: test.session_configured.model.clone(),
                        reasoning_effort: None,
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await
        .unwrap();

    let mut events = Vec::new();
    let terminal = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            events.push(event.clone());
            matches!(event, EventMsg::TurnComplete(_))
        },
        Duration::from_secs(10),
    )
    .await;
    let EventMsg::TurnComplete(terminal) = terminal else {
        unreachable!("predicate requires terminal completion");
    };
    assert_eq!(terminal.error, None);
    assert!(!events.iter().any(|event| matches!(
        event,
        EventMsg::Error(_) | EventMsg::StreamError(_) | EventMsg::TurnAborted(_)
    )));
    let starts: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            EventMsg::ItemStarted(start) => match &start.item {
                TurnItem::AgentMessage(item) => Some((index, start, item)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    let completions: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match event {
            EventMsg::ItemCompleted(completed) => match &completed.item {
                TurnItem::AgentMessage(item) => Some((index, completed, item)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(starts.len(), 1);
    assert_eq!(
        completions.len(),
        1,
        "an empty final payload must still close the started item once"
    );
    let (start_index, start, started_item) = starts[0];
    let (completed_index, completed, completed_item) = completions[0];
    assert_eq!(started_item.id, "assistant-empty-final");
    assert_eq!(completed_item.id, started_item.id);
    assert_eq!(start.turn_id, terminal.turn_id);
    assert_eq!(completed.turn_id, terminal.turn_id);
    assert!(start_index < completed_index && completed_index < events.len() - 1);
    let mut visible_text = String::new();
    for (index, event) in events.iter().enumerate() {
        if let EventMsg::AgentMessageContentDelta(delta) = event {
            assert_eq!(delta.item_id, started_item.id);
            assert_eq!(delta.turn_id, terminal.turn_id);
            assert!(start_index < index && index < completed_index);
            visible_text.push_str(&delta.delta);
        }
    }
    assert_eq!(visible_text, "I will inspect the stream.\n");
    assert!(matches!(
        completed_item.content.as_slice(),
        [AgentMessageContent::Text { text }] if text == "I will inspect the stream.\n"
    ));
    assert_eq!(server.requests().await.len(), 1);
    server.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reasoning_closure_preserves_partial_text_and_final_authority() {
    skip_if_no_network!();

    for sequential_cutoff in [false, true] {
        for normal_done in [false, true] {
            let mut response_events = vec![
                responses::ev_response_created("resp_reasoning_partial"),
                responses::ev_reasoning_item_added("reasoning-partial", &[]),
                responses::ev_reasoning_summary_text_delta("Checking "),
                responses::ev_reasoning_summary_text_delta("the stream"),
                serde_json::json!({
                    "type": "response.reasoning_summary_text.done",
                    "item_id": "reasoning-partial",
                    "summary_index": 0,
                    "text": "Checking the stream",
                }),
                responses::ev_reasoning_text_delta("The provider "),
                responses::ev_reasoning_text_delta("has not finished"),
            ];
            if normal_done {
                response_events.extend([
                    responses::ev_reasoning_item(
                        "reasoning-partial",
                        &["Final summary"],
                        &["Final raw content"],
                    ),
                    responses::ev_completed("resp_reasoning_partial"),
                ]);
            }
            let (server, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
                gate: None,
                body: responses::sse(response_events),
            }]])
            .await;
            let test = test_codex()
                .with_config(move |config| {
                    config.model_provider.request_max_retries = Some(0);
                    config.model_provider.stream_max_retries = Some(0);
                    config.show_raw_agent_reasoning = true;
                    if sequential_cutoff {
                        let _ = config
                            .features
                            .enable(Feature::ConcurrentReasoningSummaries);
                    } else {
                        let _ = config
                            .features
                            .disable(Feature::ConcurrentReasoningSummaries);
                    }
                })
                .build_with_streaming_server(&server)
                .await
                .unwrap();
            assert_eq!(
                test.config
                    .features
                    .enabled(Feature::ConcurrentReasoningSummaries),
                sequential_cutoff
            );
            assert!(test.config.model_provider.is_openai());
            test.codex
                .submit(Op::UserInput {
                    items: vec![UserInput::Text {
                        text: "Check this response".into(),
                        text_elements: Vec::new(),
                    }],
                    final_output_json_schema: None,
                    responsesapi_client_metadata: None,
                    additional_context: Default::default(),
                    thread_settings: Default::default(),
                })
                .await
                .unwrap();

            let mut events = Vec::new();
            let terminal = wait_for_event_with_timeout(
                &test.codex,
                |event| {
                    events.push(event.clone());
                    matches!(event, EventMsg::TurnComplete(_))
                },
                Duration::from_secs(10),
            )
            .await;
            let EventMsg::TurnComplete(terminal) = terminal else {
                unreachable!("predicate requires terminal completion");
            };
            let errors: Vec<_> = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| match event {
                    EventMsg::Error(error) => Some((index, error)),
                    _ => None,
                })
                .collect();
            let terminal_index = if normal_done {
                assert!(errors.is_empty());
                assert_eq!(terminal.error, None);
                events.len() - 1
            } else {
                assert_eq!(errors.len(), 1);
                let (error_index, error) = errors[0];
                assert!(matches!(
                    error.codex_error_info,
                    Some(CodexErrorInfo::ResponseStreamConnectionFailed {
                        http_status_code: None
                    })
                ));
                assert_eq!(
                    error.message,
                    "Error while reading the server response: stream closed before response.completed"
                );
                assert_eq!(terminal.error.as_ref(), Some(error));
                error_index
            };
            assert!(
                !events.iter().any(|event| matches!(
                    event,
                    EventMsg::StreamError(_) | EventMsg::TurnAborted(_)
                ))
            );
            let (expected_summary, expected_raw) = if normal_done {
                ("Final summary", "Final raw content")
            } else {
                ("Checking the stream", "The provider has not finished")
            };
            let starts: Vec<_> = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| match event {
                    EventMsg::ItemStarted(start) => match &start.item {
                        TurnItem::Reasoning(item) => Some((index, start, item)),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            let completions: Vec<_> = events
                .iter()
                .enumerate()
                .filter_map(|(index, event)| match event {
                    EventMsg::ItemCompleted(completed) => match &completed.item {
                        TurnItem::Reasoning(item) => Some((index, completed, item)),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            assert_eq!(starts.len(), 1);
            assert_eq!(completions.len(), 1);
            let (start_index, start, started_item) = starts[0];
            let (completed_index, completed, completed_item) = completions[0];
            assert_eq!(started_item.id, "reasoning-partial");
            assert_eq!(completed_item.id, started_item.id);
            assert_eq!(start.turn_id, terminal.turn_id);
            assert_eq!(completed.turn_id, terminal.turn_id);
            assert!(start_index < completed_index && completed_index < terminal_index);
            assert_eq!(completed_item.summary_text, vec![expected_summary]);
            assert_eq!(completed_item.raw_content, vec![expected_raw]);
            let mut summary_text = String::new();
            let mut raw_text = String::new();
            for (index, event) in events.iter().enumerate() {
                match event {
                    EventMsg::ReasoningContentDelta(delta) => {
                        assert_eq!(delta.item_id, "reasoning-partial");
                        assert_eq!(delta.summary_index, 0);
                        assert!(start_index < index && index < completed_index);
                        summary_text.push_str(&delta.delta);
                    }
                    EventMsg::ReasoningRawContentDelta(delta) => {
                        assert_eq!(delta.item_id, "reasoning-partial");
                        assert_eq!(delta.content_index, 0);
                        assert!(start_index < index && index < completed_index);
                        raw_text.push_str(&delta.delta);
                    }
                    _ => {}
                }
            }
            assert_eq!(summary_text, "Checking the stream");
            assert_eq!(raw_text, "The provider has not finished");
            let mut history = ThreadHistoryBuilder::new();
            for event in &events {
                history.handle_event(event);
            }
            assert_eq!(history.in_progress_turn_id(), None);
            let turns = history.finish();
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].id, terminal.turn_id);
            assert_eq!(
                turns[0].status,
                if normal_done {
                    TurnStatus::Completed
                } else {
                    TurnStatus::Failed
                }
            );
            let reasoning: Vec<_> = turns[0]
                .items
                .iter()
                .filter_map(|item| match item {
                    ThreadItem::Reasoning {
                        summary, content, ..
                    } => Some((summary, content)),
                    _ => None,
                })
                .collect();
            assert_eq!(reasoning.len(), 1);
            assert_eq!(reasoning[0].0.as_slice(), &[expected_summary]);
            assert_eq!(reasoning[0].1.as_slice(), &[expected_raw]);
            assert_eq!(server.requests().await.len(), 1);
            server.shutdown().await;
        }
    }
}
