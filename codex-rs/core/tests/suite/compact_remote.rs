use anyhow::Result;
use codex_login::CodexAuth;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::start_websocket_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::TestCodexHarness;
use core_test_support::test_codex::test_codex as base_test_codex;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::time::Duration;
use wiremock::ResponseTemplate;

const DUMMY_FUNCTION_NAME: &str = "test_tool";
const TURN_STATE_HEADER: &str = "x-codex-turn-state";
const REMOTE_COMPACT_TURN_COMPLETE_TIMEOUT: Duration = Duration::from_secs(30);

fn test_codex() -> TestCodexBuilder {
    base_test_codex()
}

async fn wait_for_turn_complete(codex: &codex_core::CodexThread) {
    wait_for_event_with_timeout(
        codex,
        |ev| matches!(ev, EventMsg::TurnComplete(_)),
        REMOTE_COMPACT_TURN_COMPLETE_TIMEOUT,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_reuses_compaction_trigger_for_followups() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
    )
    .await?;
    let codex = harness.test().codex.clone();

    let responses_mock = responses::mount_sse_sequence(
        harness.server(),
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("m1", "FIRST_REMOTE_REPLY"),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "ENCRYPTED_CONTEXT_COMPACTION_SUMMARY",
                    }
                }),
                responses::ev_completed("resp-compact"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello remote compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    let response_requests = responses_mock.requests();
    let compact_request = &response_requests[1];
    assert!(
        compact_request
            .header("x-codex-beta-features")
            .as_deref()
            .is_none_or(|value| {
                !value
                    .split(',')
                    .any(|feature| feature == "remote_compaction_v2")
            }),
        "retired feature keys must not be advertised in the beta feature header"
    );
    assert_eq!(compact_request.path(), "/v1/responses");
    let compact_metadata: Value = serde_json::from_str(
        &compact_request
            .header("x-codex-turn-metadata")
            .expect("v2 compact request should include turn metadata"),
    )
    .expect("v2 compact turn metadata should be valid json");
    assert_eq!(
        compact_metadata["request_kind"].as_str(),
        Some("compaction")
    );
    assert_eq!(
        compact_metadata["window_id"].as_str(),
        compact_request.header("x-codex-window-id").as_deref()
    );
    assert_eq!(
        compact_request.body_json()["client_metadata"]["x-codex-window-id"].as_str(),
        compact_metadata["window_id"].as_str()
    );
    assert_eq!(
        compact_metadata["compaction"],
        json!({
            "trigger": "manual",
            "reason": "user_requested",
            "implementation": "responses_compaction_v2",
            "phase": "standalone_turn",
            "strategy": "memento",
        })
    );
    let compact_body = compact_request.body_json().to_string();
    assert!(
        compact_body.contains("\"type\":\"compaction_trigger\""),
        "expected v2 compaction request to include the compaction_trigger item"
    );
    assert!(
        !compact_body.contains("ENCRYPTED_CONTEXT_COMPACTION_SUMMARY"),
        "expected v2 compaction trigger item to omit encrypted_content"
    );

    let follow_up_request = response_requests.last().expect("follow-up request missing");
    let follow_up_body = follow_up_request.body_json().to_string();
    assert!(
        follow_up_body.contains("\"type\":\"compaction\""),
        "expected follow-up request to preserve the compaction item"
    );
    assert!(
        follow_up_body.contains("ENCRYPTED_CONTEXT_COMPACTION_SUMMARY"),
        "expected follow-up request to include the compaction payload"
    );
    assert!(
        !follow_up_body.contains("hello remote compact"),
        "expected v2 follow-up request to evict original user history consumed by compaction"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_retries_failures_with_stream_retry_budget() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_provider.request_max_retries = Some(0);
                config.model_provider.stream_max_retries = Some(2);
            }),
    )
    .await?;
    let codex = harness.test().codex.clone();

    let responses_mock = responses::mount_response_sequence(
        harness.server(),
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("m1", "FIRST_REMOTE_REPLY"),
                responses::ev_completed("resp-1"),
            ])),
            ResponseTemplate::new(500).set_body_string("first compact open failed"),
            responses::sse_response(responses::sse(vec![serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "compaction",
                    "encrypted_content": "FAILED_COMPACT_SUMMARY",
                }
            })])),
            responses::sse_response(responses::sse(vec![
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "RETRIED_COMPACT_SUMMARY",
                    }
                }),
                responses::ev_completed("resp-compact-retry"),
            ])),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ])),
        ],
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello remote compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    let response_requests = responses_mock.requests();
    assert_eq!(
        5,
        response_requests.len(),
        "expected initial turn, failed open, failed stream, compact retry, and follow-up turn"
    );

    for compact_request in &response_requests[1..=3] {
        assert_eq!("/v1/responses", compact_request.path());
        assert!(
            compact_request
                .body_json()
                .to_string()
                .contains("\"type\":\"compaction_trigger\""),
            "expected v2 compaction request to include the compaction_trigger item"
        );
    }

    let follow_up_request = response_requests.last().expect("follow-up request missing");
    let follow_up_body = follow_up_request.body_json().to_string();
    assert!(
        follow_up_body.contains("RETRIED_COMPACT_SUMMARY"),
        "expected follow-up request to include the retried compaction payload"
    );
    assert!(
        !follow_up_body.contains("FAILED_COMPACT_SUMMARY"),
        "expected failed compaction attempt output to be discarded"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_compact_v2_accepts_additional_output_items_before_compaction() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex().with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
    )
    .await?;
    let codex = harness.test().codex.clone();

    let responses_mock = responses::mount_sse_sequence(
        harness.server(),
        vec![
            responses::sse(vec![
                responses::ev_assistant_message("m1", "FIRST_REMOTE_REPLY"),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m-compact-noise", "IGNORED_COMPACT_REPLY"),
                serde_json::json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "ENCRYPTED_CONTEXT_COMPACTION_SUMMARY",
                    }
                }),
                responses::ev_completed("resp-compact"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("m2", "AFTER_COMPACT_REPLY"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello remote compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    codex.submit(Op::Compact).await?;
    wait_for_turn_complete(&codex).await;

    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "after compact".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    let response_requests = responses_mock.requests();
    let follow_up_request = response_requests.last().expect("follow-up request missing");
    let follow_up_body = follow_up_request.body_json().to_string();
    assert!(
        follow_up_body.contains("\"type\":\"compaction\""),
        "expected follow-up request to preserve the compaction item"
    );
    assert!(
        follow_up_body.contains("ENCRYPTED_CONTEXT_COMPACTION_SUMMARY"),
        "expected follow-up request to include the compaction payload"
    );
    assert!(
        !follow_up_body.contains("IGNORED_COMPACT_REPLY"),
        "expected follow-up request to ignore unrelated output items from the compaction stream"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mid_turn_compact_v2_sends_turn_state_over_http() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let harness = TestCodexHarness::with_builder(
        test_codex()
            .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
            .with_config(|config| {
                config.model_auto_compact_token_limit = Some(200_000);
            }),
    )
    .await?;
    let codex = harness.test().codex.clone();
    let responses_mock = responses::mount_response_sequence(
        harness.server(),
        vec![
            responses::sse_response(responses::sse(vec![
                responses::ev_function_call("call-before-compact", DUMMY_FUNCTION_NAME, "{}"),
                responses::ev_completed_with_tokens("r1", /*total_tokens*/ 500_000),
            ]))
            .insert_header(TURN_STATE_HEADER, "sampling-state"),
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "compaction",
                        "encrypted_content": "V2_COMPACT_SUMMARY",
                    }
                }),
                responses::ev_completed("r-compact"),
            ]))
            .insert_header(TURN_STATE_HEADER, "compact-state"),
            responses::sse_response(responses::sse(vec![
                responses::ev_function_call("call-after-compact", DUMMY_FUNCTION_NAME, "{}"),
                responses::ev_completed_with_tokens("r2", /*total_tokens*/ 80_000),
            ]))
            .insert_header(TURN_STATE_HEADER, "continuation-state"),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("m1", "FINAL_REPLY"),
                responses::ev_completed_with_tokens("r3", /*total_tokens*/ 80_000),
            ])),
        ],
    )
    .await;

    // Phase 1: sampling mints state and schedules inline v2 compaction.
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "RUN_WITH_MID_TURN_COMPACT_V2".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&codex).await;

    let requests = responses_mock.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests
            .iter()
            .all(|request| request.path() == "/v1/responses")
    );
    assert_eq!(requests[0].header(TURN_STATE_HEADER), None);

    // Phase 2: the v2 compaction request replays the state already established by sampling.
    assert!(
        requests[1]
            .body_json()
            .to_string()
            .contains("\"type\":\"compaction_trigger\"")
    );
    assert_eq!(
        requests[1].header(TURN_STATE_HEADER).as_deref(),
        Some("sampling-state")
    );

    // Phase 3: later response headers do not replace the first value in the OnceLock.
    assert_eq!(
        requests[2].header(TURN_STATE_HEADER).as_deref(),
        Some("sampling-state")
    );
    assert_eq!(
        requests[3].header(TURN_STATE_HEADER).as_deref(),
        Some("sampling-state")
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_mid_turn_compact_v2_sends_turn_state_over_websocket() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_websocket_server(vec![vec![
        vec![
            responses::ev_response_created("warm-1"),
            responses::ev_completed("warm-1"),
        ],
        vec![
            json!({
                "type": "response.metadata",
                "headers": {(TURN_STATE_HEADER): "sampling-state"},
            }),
            responses::ev_function_call("call-before-compact", DUMMY_FUNCTION_NAME, "{}"),
            responses::ev_completed_with_tokens("r1", /*total_tokens*/ 500_000),
        ],
        vec![
            json!({
                "type": "response.metadata",
                "headers": {(TURN_STATE_HEADER): "compact-state"},
            }),
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "compaction",
                    "encrypted_content": "V2_WS_COMPACT_SUMMARY",
                }
            }),
            responses::ev_completed("r-compact"),
        ],
        vec![
            json!({
                "type": "response.metadata",
                "headers": {(TURN_STATE_HEADER): "continuation-state"},
            }),
            responses::ev_function_call("call-after-compact", DUMMY_FUNCTION_NAME, "{}"),
            responses::ev_completed_with_tokens("r2", /*total_tokens*/ 80_000),
        ],
        vec![
            responses::ev_assistant_message("m1", "FINAL_REPLY"),
            responses::ev_completed_with_tokens("r3", /*total_tokens*/ 80_000),
        ],
        vec![
            responses::ev_assistant_message("m2", "NEXT_TURN_REPLY"),
            responses::ev_completed_with_tokens("r4", /*total_tokens*/ 80_000),
        ],
    ]])
    .await;
    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.model_auto_compact_token_limit = Some(200_000);
        });
    let test = builder.build_with_websocket_server(&server).await?;

    // Phase 1: startup prewarm stays empty, then WebSocket sampling mints state and schedules
    // inline v2 compaction.
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "RUN_WITH_WS_MID_TURN_COMPACT_V2".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&test.codex).await;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "NEXT_TURN_AFTER_WS_COMPACT_V2".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    wait_for_turn_complete(&test.codex).await;

    let requests = server.single_connection();
    assert_eq!(server.handshakes().len(), 1);
    assert_eq!(requests.len(), 6);
    assert_eq!(requests[0].body_json()["generate"].as_bool(), Some(false));
    // Phase 2: the v2 compact request replays the state already established by sampling.
    assert!(
        requests[2]
            .body_json()
            .to_string()
            .contains("\"type\":\"compaction_trigger\"")
    );
    // Phase 3: both post-compact requests keep replaying that first value.
    assert_eq!(
        requests
            .iter()
            .map(|request| request.body_json()["client_metadata"][TURN_STATE_HEADER].clone())
            .collect::<Vec<_>>(),
        vec![
            json!(null),
            json!(null),
            json!("sampling-state"),
            json!("sampling-state"),
            json!("sampling-state"),
            json!(null),
        ]
    );
    // Phase 4: a new logical turn keeps the healthy socket but resets response-chain and
    // turn-scoped state, so the compacted history is sent as a fresh request.
    let next_turn = requests[5].body_json();
    assert_eq!(next_turn.get("previous_response_id"), None);
    let next_turn_body = next_turn.to_string();
    assert!(next_turn_body.contains("V2_WS_COMPACT_SUMMARY"));
    assert!(next_turn_body.contains("NEXT_TURN_AFTER_WS_COMPACT_V2"));

    server.shutdown().await;
    Ok(())
}
