use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::create_fake_parented_rollout_with_source;
use app_test_support::create_fake_rollout;
use app_test_support::to_response;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::SessionSource as ApiSessionSource;
use codex_app_server_protocol::ThreadArchiveParams;
use codex_app_server_protocol::ThreadArchiveResponse;
use codex_app_server_protocol::ThreadForkParams;
use codex_app_server_protocol::ThreadForkResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadSource;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadUnarchiveParams;
use codex_app_server_protocol::ThreadUnarchiveResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::TurnSteerParams;
use codex_app_server_protocol::TurnSteerResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_protocol::ThreadId as CoreThreadId;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use core_test_support::AcceptedCompletionProofFixture;
use core_test_support::BlockedCompletionProofFixture;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::path::Path;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

// Loaded CI hosts can spend tens of seconds starting app-server subprocesses or
// processing turn RPCs under load.
const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const CANONICAL_PROOF_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);
const BLOCKED_COLD_RESUME_PROMPT: &str = "finish without running certification";
const TOTAL_GENERATIONS_WITH_FORCED_TERMINAL: u64 = 33;

async fn read_turn_completed(
    mcp: &mut TestAppServer,
    read_timeout: std::time::Duration,
) -> Result<TurnCompletedNotification> {
    let notification = timeout(
        read_timeout,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    Ok(serde_json::from_value(notification.params.expect(
        "turn/completed notification should include params",
    ))?)
}

async fn start_text_turn(mcp: &mut TestAppServer, thread_id: String, text: &str) -> Result<()> {
    let turn_id = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id,
            input: vec![V2UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_id)),
    )
    .await??;
    Ok(())
}

#[tokio::test]
async fn turn_start_forwards_client_metadata_to_responses_request_v2() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "Done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &server.uri(),
        /*supports_websockets*/ false,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            thread_source: Some(ThreadSource::Feature("automation".to_string())),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let client_metadata = HashMap::from([
        ("fiber_run_id".to_string(), "fiber-start-123".to_string()),
        ("origin".to_string(), "gaas".to_string()),
    ]);
    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "Hello".to_string(),
                text_elements: Vec::new(),
            }],
            responsesapi_client_metadata: Some(client_metadata.clone()),
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let request = response_mock.single_request();
    let metadata = request
        .header("x-codex-turn-metadata")
        .as_deref()
        .map(parse_json_header)
        .expect("x-codex-turn-metadata header should be present");
    assert_eq!(metadata["fiber_run_id"].as_str(), Some("fiber-start-123"));
    assert_eq!(metadata["origin"].as_str(), Some("gaas"));
    assert_eq!(metadata["thread_source"].as_str(), Some("automation"));
    assert_eq!(metadata["turn_id"].as_str(), Some(turn.id.as_str()));
    assert!(metadata.get("installation_id").is_some());
    assert!(metadata.get("session_id").is_some());
    assert_eq!(
        metadata["window_id"].as_str(),
        request.header("x-codex-window-id").as_deref()
    );

    Ok(())
}

#[tokio::test]
async fn turn_start_sends_fork_lineage_in_turn_metadata_for_thread_fork_v2() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "Done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &server.uri(),
        /*supports_websockets*/ false,
    )?;

    let source_thread_id = create_fake_rollout(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "Saved user message",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let ThreadForkResponse { thread, .. } =
        fork_fake_rollout_thread(&mut mcp, source_thread_id.clone()).await?;

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "Continue".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let request = response_mock.single_request();
    let metadata = request
        .header("x-codex-turn-metadata")
        .as_deref()
        .map(parse_json_header)
        .expect("x-codex-turn-metadata header should be present");
    assert_eq!(
        metadata["forked_from_thread_id"].as_str(),
        Some(source_thread_id.as_str())
    );
    assert_eq!(metadata["thread_id"].as_str(), Some(thread.id.as_str()));
    assert_eq!(metadata["turn_id"].as_str(), Some(turn.id.as_str()));

    Ok(())
}

#[tokio::test]
async fn turn_start_sends_nested_subagent_lineage_after_cold_thread_resume_v2() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "Done"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &server.uri(),
        /*supports_websockets*/ false,
    )?;

    let root_thread_id = CoreThreadId::new();
    let root_thread_id_str = root_thread_id.to_string();
    let parent_thread_id = CoreThreadId::new();
    let parent_thread_id_str = parent_thread_id.to_string();
    let subagent_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "Saved subagent message",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::Other("guardian".to_string())),
        root_thread_id.into(),
        parent_thread_id,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let resume_req = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: subagent_thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let resume_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(resume_req)),
    )
    .await??;
    let ThreadResumeResponse { thread, .. } = to_response::<ThreadResumeResponse>(resume_resp)?;
    assert_eq!(thread.id, subagent_thread_id);
    assert_eq!(thread.session_id, root_thread_id_str);
    assert_eq!(thread.parent_thread_id, Some(parent_thread_id_str.clone()));
    assert_eq!(
        thread.source,
        ApiSessionSource::SubAgent(SubAgentSource::Other("guardian".to_string()))
    );

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![V2UserInput::Text {
                text: "Continue".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let request = response_mock.single_request();
    let metadata = request
        .header("x-codex-turn-metadata")
        .as_deref()
        .map(parse_json_header)
        .expect("x-codex-turn-metadata header should be present");
    assert_eq!(
        metadata["parent_thread_id"].as_str(),
        Some(parent_thread_id_str.as_str())
    );
    assert_eq!(metadata["subagent_kind"].as_str(), Some("guardian"));
    assert_eq!(
        metadata["session_id"].as_str(),
        Some(thread.session_id.as_str())
    );
    assert_eq!(metadata["thread_id"].as_str(), Some(thread.id.as_str()));
    assert_eq!(metadata["turn_id"].as_str(), Some(turn.id.as_str()));
    assert!(metadata.get("forked_from_thread_id").is_none());

    Ok(())
}

#[tokio::test]
async fn cold_resume_of_legacy_internal_source_still_requires_root_completion_proof_v2()
-> Result<()> {
    let fixture = BlockedCompletionProofFixture::new()?;
    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(|request: &wiremock::Request| {
            String::from_utf8_lossy(&request.body).contains(BLOCKED_COLD_RESUME_PROMPT)
        })
        .respond_with(responses::sse_response(responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "premature success"),
            responses::ev_completed("resp-1"),
        ])))
        .up_to_n_times(TOTAL_GENERATIONS_WITH_FORCED_TERMINAL)
        .expect(TOTAL_GENERATIONS_WITH_FORCED_TERMINAL)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &server.uri(),
        /*supports_websockets*/ false,
    )?;
    let legacy_internal_thread_id = create_fake_parented_rollout_with_source(
        codex_home.path(),
        "2025-01-05T12-00-00",
        "2025-01-05T12:00:00Z",
        "Saved legacy internal task message",
        Some("mock_provider"),
        /*git_info*/ None,
        SessionSource::SubAgent(SubAgentSource::Other("legacy-internal".to_string())),
        CoreThreadId::new().into(),
        CoreThreadId::new(),
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let resume_req = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: legacy_internal_thread_id,
            cwd: Some(fixture.repo_path().display().to_string()),
            ..Default::default()
        })
        .await?;
    let resume_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(resume_req)),
    )
    .await??;
    let ThreadResumeResponse { thread, .. } = to_response::<ThreadResumeResponse>(resume_resp)?;
    assert_eq!(
        thread.source,
        ApiSessionSource::SubAgent(SubAgentSource::Other("legacy-internal".to_string()))
    );

    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            input: vec![V2UserInput::Text {
                text: BLOCKED_COLD_RESUME_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;

    let (completed_notification, observed_payloads) = timeout(DEFAULT_READ_TIMEOUT, async {
        let mut observed_payloads = Vec::new();
        loop {
            let message = mcp.read_next_message().await?;
            observed_payloads.push(serde_json::to_string(&message)?);
            if matches!(
                &message,
                JSONRPCMessage::Notification(notification)
                    if notification.method == "turn/completed"
            ) {
                let JSONRPCMessage::Notification(notification) = message else {
                    unreachable!("turn/completed match must be a notification")
                };
                break Ok::<_, anyhow::Error>((notification, observed_payloads));
            }
        }
    })
    .await??;

    assert!(
        observed_payloads
            .iter()
            .all(|payload| !payload.contains("premature success")),
        "a public cold resume of legacy internal metadata published buffered terminal output: {observed_payloads:#?}"
    );
    let completed: TurnCompletedNotification = serde_json::from_value(
        completed_notification
            .params
            .expect("turn/completed notification should include params"),
    )?;
    assert_eq!(completed.turn.status, TurnStatus::Failed);
    assert_eq!(completed.turn.surfaced_result, None);
    assert_eq!(completed.surfaced_result, None);
    let error = completed
        .turn
        .error
        .expect("blocked completion should include the terminal gate error");
    assert!(
        error
            .message
            .contains("CompletionProofGate blocked terminal success"),
        "unexpected app-server terminal error: {}",
        error.message
    );
    assert!(
        !fixture.canonical_runner_launched(),
        "the CompletionProofGate launched the canonical runner"
    );

    Ok(())
}

#[tokio::test]
async fn same_process_resume_reuses_completion_proof_but_restart_does_not_v2() -> Result<()> {
    let fixture = AcceptedCompletionProofFixture::new()?;
    let server = responses::start_mock_server().await;
    let exec_arguments = serde_json::to_string(&serde_json::json!({
        "kind": "script",
        "cmd": fixture.canonical_command(),
        "workdir": fixture.repo_path().to_string_lossy(),
        "tty": false,
        "yield_time_ms": 30_000,
    }))?;
    let mut response_sequence = vec![
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("proof-exec-response"),
            responses::ev_function_call("proof-exec-call", "exec_command", &exec_arguments),
            responses::ev_completed("proof-exec-response"),
        ])),
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("initial-terminal-response"),
            responses::ev_assistant_message(
                "initial-terminal-message",
                "initial app-server proof accepted",
            ),
            responses::ev_completed("initial-terminal-response"),
        ])),
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created("same-process-terminal-response"),
            responses::ev_assistant_message(
                "same-process-terminal-message",
                "same-process resume retained live issuance",
            ),
            responses::ev_completed("same-process-terminal-response"),
        ])),
    ];
    response_sequence.extend((0..TOTAL_GENERATIONS_WITH_FORCED_TERMINAL).map(|index| {
        let response_id = format!("restart-terminal-response-{index}");
        let message_id = format!("restart-terminal-message-{index}");
        responses::sse_response(responses::sse(vec![
            responses::ev_response_created(&response_id),
            responses::ev_assistant_message(&message_id, "restart must not publish this output"),
            responses::ev_completed(&response_id),
        ]))
    }));
    responses::mount_response_sequence(&server, response_sequence).await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &server.uri(),
        /*supports_websockets*/ false,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;
    let mut environment = mcp.auto_env_params()?;
    environment.cwd =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(fixture.repo_path().to_path_buf())?
            .into();

    let start_id = mcp
        .send_thread_start_request(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some(fixture.repo_path().display().to_string()),
            environments: Some(vec![environment]),
            approval_policy: Some(AskForApproval::Never),
            sandbox: Some(SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .await?;
    let start_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(start_response)?;
    let thread_id = thread.id;

    start_text_turn(
        &mut mcp,
        thread_id.clone(),
        "run the canonical completion proof and finish",
    )
    .await?;
    assert_eq!(
        read_turn_completed(&mut mcp, CANONICAL_PROOF_READ_TIMEOUT)
            .await?
            .turn
            .status,
        TurnStatus::Completed
    );
    assert!(fixture.canonical_runner_launched());

    let archive_id = mcp
        .send_thread_archive_request(ThreadArchiveParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let archive_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(archive_id)),
    )
    .await??;
    let _: ThreadArchiveResponse = to_response(archive_response)?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("thread/archived"),
    )
    .await??;

    let unarchive_id = mcp
        .send_thread_unarchive_request(ThreadUnarchiveParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let unarchive_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(unarchive_id)),
    )
    .await??;
    let _: ThreadUnarchiveResponse = to_response(unarchive_response)?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("thread/unarchived"),
    )
    .await??;

    let resume_id = mcp
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            developer_instructions: Some(
                "reconstruct settings while preserving live issuance".to_string(),
            ),
            ..Default::default()
        })
        .await?;
    let resume_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(resume_id)),
    )
    .await??;
    let ThreadResumeResponse { thread, .. } = to_response::<ThreadResumeResponse>(resume_response)?;
    assert_eq!(thread.id, thread_id);

    start_text_turn(
        &mut mcp,
        thread_id.clone(),
        "finish without rerunning the canonical command",
    )
    .await?;
    assert_eq!(
        read_turn_completed(&mut mcp, DEFAULT_READ_TIMEOUT)
            .await?
            .turn
            .status,
        TurnStatus::Completed
    );

    mcp.close_stdin();
    let exit = timeout(DEFAULT_READ_TIMEOUT, mcp.wait_for_exit()).await??;
    assert!(
        exit.success(),
        "first app-server process did not exit cleanly"
    );
    drop(mcp);

    let mut restarted = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, restarted.initialize()).await??;
    let restart_resume_id = restarted
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    let restart_resume_response: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        restarted.read_stream_until_response_message(RequestId::Integer(restart_resume_id)),
    )
    .await??;
    let ThreadResumeResponse { thread, .. } =
        to_response::<ThreadResumeResponse>(restart_resume_response)?;
    assert_eq!(thread.id, thread_id);

    start_text_turn(
        &mut restarted,
        thread_id,
        "finish without rerunning certification",
    )
    .await?;
    let (completed_notification, observed_payloads) = timeout(DEFAULT_READ_TIMEOUT, async {
        let mut observed_payloads = Vec::new();
        loop {
            let message = restarted.read_next_message().await?;
            observed_payloads.push(serde_json::to_string(&message)?);
            if matches!(
                &message,
                JSONRPCMessage::Notification(notification)
                    if notification.method == "turn/completed"
            ) {
                let JSONRPCMessage::Notification(notification) = message else {
                    unreachable!("turn/completed match must be a notification")
                };
                break Ok::<_, anyhow::Error>((notification, observed_payloads));
            }
        }
    })
    .await??;
    assert!(
        observed_payloads
            .iter()
            .all(|payload| !payload.contains("restart must not publish this output")),
        "a restarted app-server recovered process-local issuance: {observed_payloads:#?}"
    );
    let completed: TurnCompletedNotification = serde_json::from_value(
        completed_notification
            .params
            .expect("turn/completed notification should include params"),
    )?;
    assert_eq!(completed.turn.status, TurnStatus::Failed);
    assert_eq!(completed.turn.surfaced_result, None);
    assert_eq!(completed.surfaced_result, None);
    let error = completed
        .turn
        .error
        .expect("blocked restart should include the terminal gate error");
    assert!(
        error
            .message
            .contains("CompletionProofGate blocked terminal success"),
        "unexpected app-server restart terminal error: {}",
        error.message
    );

    Ok(())
}

#[tokio::test]
async fn turn_steer_updates_client_metadata_on_follow_up_responses_request_v2() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let codex_home = TempDir::new()?;

    let server = responses::start_mock_server().await;
    let first_response = responses::sse_response(responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "Working"),
        responses::ev_completed("resp-1"),
    ]))
    .set_delay(std::time::Duration::from_secs(2));
    let second_response = responses::sse_response(responses::sse(vec![
        responses::ev_response_created("resp-2"),
        responses::ev_assistant_message("msg-2", "Done"),
        responses::ev_completed("resp-2"),
    ]));
    let request_log =
        responses::mount_response_sequence(&server, vec![first_response, second_response]).await;

    create_config_toml(
        codex_home.path(),
        &server.uri(),
        /*supports_websockets*/ false,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let start_metadata =
        HashMap::from([("fiber_run_id".to_string(), "fiber-start-123".to_string())]);
    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "Run sleep".to_string(),
                text_elements: Vec::new(),
            }],
            responsesapi_client_metadata: Some(start_metadata.clone()),
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;
    let turn_id = turn.id.clone();

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/started"),
    )
    .await??;
    wait_for_request_count(&request_log, /*expected*/ 1).await?;

    let steer_metadata = HashMap::from([
        ("fiber_run_id".to_string(), "fiber-steer-456".to_string()),
        ("origin".to_string(), "gaas".to_string()),
    ]);
    let steer_req = mcp
        .send_turn_steer_request(TurnSteerParams {
            thread_id: thread.id.clone(),
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "Focus on the failure".to_string(),
                text_elements: Vec::new(),
            }],
            responsesapi_client_metadata: Some(steer_metadata.clone()),
            additional_context: None,
            expected_turn_id: turn_id.clone(),
        })
        .await?;
    let steer_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(steer_req)),
    )
    .await??;
    let _turn: TurnSteerResponse = to_response::<TurnSteerResponse>(steer_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let requests = request_log.requests();
    assert_eq!(requests.len(), 2);
    let first_metadata = requests[0]
        .header("x-codex-turn-metadata")
        .as_deref()
        .map(parse_json_header)
        .expect("first x-codex-turn-metadata header should be present");
    assert_eq!(
        first_metadata["fiber_run_id"].as_str(),
        Some("fiber-start-123")
    );
    assert_eq!(first_metadata["turn_id"].as_str(), Some(turn_id.as_str()));

    let second_metadata = requests[1]
        .header("x-codex-turn-metadata")
        .as_deref()
        .map(parse_json_header)
        .expect("second x-codex-turn-metadata header should be present");
    assert_eq!(
        second_metadata["fiber_run_id"].as_str(),
        Some("fiber-steer-456")
    );
    assert_eq!(second_metadata["origin"].as_str(), Some("gaas"));
    assert_eq!(second_metadata["turn_id"].as_str(), Some(turn_id.as_str()));

    Ok(())
}

#[tokio::test]
async fn turn_start_forwards_client_metadata_to_responses_websocket_request_body_v2() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    let websocket_server = responses::start_websocket_server(vec![vec![
        vec![
            responses::ev_response_created("warm-1"),
            responses::ev_completed("warm-1"),
        ],
        vec![
            responses::ev_response_created("resp-1"),
            responses::ev_assistant_message("msg-1", "Done"),
            responses::ev_completed("resp-1"),
        ],
    ]])
    .await;

    let codex_home = TempDir::new()?;
    create_config_toml(
        codex_home.path(),
        &websocket_server.uri().replacen("ws://", "http://", 1),
        /*supports_websockets*/ true,
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_READ_TIMEOUT, mcp.initialize()).await??;

    let thread_req = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            thread_source: Some(ThreadSource::Feature("automation".to_string())),
            ..Default::default()
        })
        .await?;
    let thread_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(thread_req)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response::<ThreadStartResponse>(thread_resp)?;

    let client_metadata = HashMap::from([
        ("fiber_run_id".to_string(), "fiber-start-123".to_string()),
        ("origin".to_string(), "gaas".to_string()),
    ]);
    let turn_req = mcp
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id,
            client_user_message_id: None,
            input: vec![V2UserInput::Text {
                text: "Hello".to_string(),
                text_elements: Vec::new(),
            }],
            responsesapi_client_metadata: Some(client_metadata),
            ..Default::default()
        })
        .await?;
    let turn_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(turn_req)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response::<TurnStartResponse>(turn_resp)?;

    timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_notification_message("turn/completed"),
    )
    .await??;

    let warmup = websocket_server
        .wait_for_request(/*connection_index*/ 0, /*request_index*/ 0)
        .await
        .body_json();
    let request = websocket_server
        .wait_for_request(/*connection_index*/ 0, /*request_index*/ 1)
        .await
        .body_json();

    assert_eq!(warmup["type"].as_str(), Some("response.create"));
    assert_eq!(warmup["generate"].as_bool(), Some(false));
    assert_eq!(request["type"].as_str(), Some("response.create"));
    assert_eq!(request["previous_response_id"].as_str(), Some("warm-1"));

    let metadata = request["client_metadata"]["x-codex-turn-metadata"]
        .as_str()
        .map(parse_json_header)
        .expect("websocket x-codex-turn-metadata client metadata should be present");
    assert_eq!(metadata["fiber_run_id"].as_str(), Some("fiber-start-123"));
    assert_eq!(metadata["origin"].as_str(), Some("gaas"));
    assert_eq!(metadata["thread_source"].as_str(), Some("automation"));
    assert_eq!(metadata["turn_id"].as_str(), Some(turn.id.as_str()));
    assert!(metadata.get("session_id").is_some());
    assert_eq!(
        metadata["window_id"].as_str(),
        request["client_metadata"]["x-codex-window-id"].as_str()
    );

    websocket_server.shutdown().await;
    Ok(())
}

fn create_config_toml(
    codex_home: &Path,
    server_uri: &str,
    supports_websockets: bool,
) -> std::io::Result<()> {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        config_toml,
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "read-only"

model_provider = "mock_provider"

[model_providers.mock_provider]
name = "Mock provider for test"
base_url = "{server_uri}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = {supports_websockets}
"#
        ),
    )
}

async fn fork_fake_rollout_thread(
    mcp: &mut TestAppServer,
    source_thread_id: String,
) -> Result<ThreadForkResponse> {
    let fork_req = mcp
        .send_thread_fork_request(ThreadForkParams {
            thread_id: source_thread_id,
            thread_source: Some(ThreadSource::User),
            ..Default::default()
        })
        .await?;
    let fork_resp: JSONRPCResponse = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(fork_req)),
    )
    .await??;
    to_response::<ThreadForkResponse>(fork_resp)
}

fn parse_json_header(value: &str) -> serde_json::Value {
    serde_json::from_str(value).expect("metadata header should contain valid JSON")
}

async fn wait_for_request_count(
    request_log: &core_test_support::responses::ResponseMock,
    expected: usize,
) -> Result<()> {
    timeout(DEFAULT_READ_TIMEOUT, async {
        loop {
            if request_log.requests().len() >= expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(())
}
