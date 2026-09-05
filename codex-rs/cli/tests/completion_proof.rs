use anyhow::Result;
use app_test_support::AcceptedCompletionProofFixture;
use app_test_support::BlockedCompletionProofFixture;
use app_test_support::TestAppServer;
use app_test_support::core_responses;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::create_mock_responses_server_sequence;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::AskForApproval;
use codex_app_server_protocol::CommandExecutionStatus;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::ItemStartedNotification;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SandboxMode;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnCompletedNotification;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use std::collections::BTreeMap;
#[cfg(windows)]
use std::process::Stdio;
use tempfile::TempDir;
#[cfg(windows)]
use tokio::process::Child;
#[cfg(windows)]
use tokio::process::Command;
use tokio::time::Duration;
use tokio::time::timeout;
#[cfg(windows)]
use wiremock::Mock;
#[cfg(windows)]
use wiremock::MockServer;
#[cfg(windows)]
use wiremock::ResponseTemplate;
#[cfg(windows)]
use wiremock::matchers::method;
#[cfg(windows)]
use wiremock::matchers::path_regex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_proof_block_hides_success_and_fails_real_cli_exec() -> Result<()> {
    let fixture = BlockedCompletionProofFixture::new()?;
    let server = create_mock_responses_server_repeating_assistant("premature success").await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::new(),
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;

    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    let output = command
        .env("CODEX_HOME", codex_home.path())
        .current_dir(fixture.repo_path())
        .args([
            "exec",
            "--skip-git-repo-check",
            "finish without running certification",
        ])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "blocked terminal completion unexpectedly succeeded; stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("premature success") && !stderr.contains("premature success"),
        "CLI exposed the buffered assistant success; stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        stderr.contains("CompletionProofGate blocked terminal success"),
        "CLI did not surface the completion-proof failure; stderr={stderr:?}"
    );
    assert!(
        !fixture.canonical_runner_launched(),
        "CLI must not launch canonical certification"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_cli_app_server_accepts_canonical_completion_proof_via_direct_argv() -> Result<()> {
    const TERMINAL_TEXT: &str = "real CLI app-server accepted direct argv proof";
    const ACCEPTED_PROOF_LINE: &str = "Completion proof accepted for the current workspace.";
    const READ_TIMEOUT: Duration = Duration::from_secs(180);

    // The runner deliberately emits unsorted nested report objects. The real CLI
    // enables serde_json preserve_order through TUI, exercising canonical hashing.
    let fixture = AcceptedCompletionProofFixture::new()?;
    let (program, proof_script) = fixture
        .canonical_command()
        .split_once(' ')
        .ok_or_else(|| anyhow::anyhow!("fixture canonical command should contain one argument"))?;
    let exec_arguments = serde_json::to_string(&serde_json::json!({
        "kind": "argv",
        "program": program,
        "args": [proof_script],
        "workdir": fixture.repo_path().to_string_lossy(),
        "tty": false,
        "yield_time_ms": 30_000,
    }))?;
    let mut premature_terminal_candidate =
        core_responses::ev_assistant_message("pre-proof-terminal-candidate", TERMINAL_TEXT);
    premature_terminal_candidate["item"]["phase"] = serde_json::json!("final_answer");
    let exec_response = core_responses::sse(vec![
        core_responses::ev_response_created("proof-exec-response"),
        premature_terminal_candidate,
        core_responses::ev_function_call("proof-exec-call", "exec_command", &exec_arguments),
        core_responses::ev_completed("proof-exec-response"),
    ]);
    let terminal_response = core_responses::sse(vec![
        core_responses::ev_response_created("proof-terminal-release-response"),
        core_responses::ev_completed("proof-terminal-release-response"),
    ]);
    let server =
        create_mock_responses_server_sequence(vec![exec_response, terminal_response]).await;
    let codex_home = TempDir::new()?;
    // Plugin startup otherwise clones the curated marketplace from live services.
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::from([(Feature::Plugins, false)]),
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;

    let codex_bin = codex_utils_cargo_bin::cargo_bin("codex")?;
    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_program(&codex_bin)
        .with_plugin_startup_tasks()
        .with_env_overrides(&[("CODEX_CODE_MODE_HOST_PATH", None)])
        .with_args(&["app-server"])
        .build()
        .await?;
    timeout(READ_TIMEOUT, app_server.initialize()).await??;
    let mut environment = app_server.auto_env_params()?;
    environment.cwd =
        codex_utils_absolute_path::AbsolutePathBuf::try_from(fixture.repo_path().to_path_buf())?
            .into();
    #[cfg(windows)]
    let sandbox_temp = {
        // Canonical certification grants write access to TEMP/TMP during Windows
        // sandbox preparation. Keep that setup confined to this fixture.
        let sandbox_temp = fixture.repo_path().join(".fixture-state/sandbox-temp");
        std::fs::create_dir_all(&sandbox_temp)?;
        sandbox_temp.to_string_lossy().into_owned()
    };
    let start_id = app_server
        .send_thread_start_request(ThreadStartParams {
            model: Some("compact".to_string()),
            cwd: Some(fixture.repo_path().display().to_string()),
            environments: Some(vec![environment]),
            approval_policy: Some(AskForApproval::Never),
            sandbox: Some(SandboxMode::DangerFullAccess),
            #[cfg(windows)]
            config: Some(std::collections::HashMap::from([(
                "shell_environment_policy".to_string(),
                serde_json::json!({ "set": { "TEMP": sandbox_temp, "TMP": sandbox_temp } }),
            )])),
            ..Default::default()
        })
        .await?;
    let start_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(start_id)),
    )
    .await??;
    let ThreadStartResponse { thread, .. } = to_response(start_response)?;

    let turn_id = app_server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "run the canonical completion proof and finish".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn_response = timeout(
        READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(turn_id)),
    )
    .await??;
    let TurnStartResponse { turn } = to_response(turn_response)?;
    let mut started_items = Vec::new();
    let mut completed_items = Vec::new();
    let mut pre_acceptance_payloads = Vec::new();
    let mut canonical_completion_observed = false;
    let proof_result_request: serde_json::Value = timeout(READ_TIMEOUT, async {
        loop {
            if let Some(requests) = server.received_requests().await
                && let Some(request) = requests
                    .iter()
                    .filter(|request| {
                        request.method == "POST" && request.url.path().ends_with("/responses")
                    })
                    .nth(1)
            {
                break Ok::<_, anyhow::Error>(request.body_json()?);
            }
            if let Ok(message) =
                timeout(Duration::from_millis(50), app_server.read_next_message()).await
            {
                let message = message?;
                if !canonical_completion_observed {
                    pre_acceptance_payloads.push(serde_json::to_string(&message)?);
                }
                let JSONRPCMessage::Notification(notification) = message else {
                    continue;
                };
                match notification.method.as_str() {
                    "item/started" => {
                        let payload: ItemStartedNotification = serde_json::from_value(
                            notification
                                .params
                                .expect("item/started notification should include params"),
                        )?;
                        if payload.thread_id == thread.id && payload.turn_id == turn.id {
                            started_items.push(payload.item);
                        }
                    }
                    "item/completed" => {
                        let payload: ItemCompletedNotification = serde_json::from_value(
                            notification
                                .params
                                .expect("item/completed notification should include params"),
                        )?;
                        if payload.thread_id == thread.id && payload.turn_id == turn.id {
                            if matches!(
                                &payload.item,
                                ThreadItem::CommandExecution { id, .. }
                                    if id == "proof-exec-call"
                            ) {
                                canonical_completion_observed = true;
                            }
                            completed_items.push(payload.item);
                        }
                    }
                    "turn/completed" => {
                        let payload: TurnCompletedNotification = serde_json::from_value(
                            notification
                                .params
                                .expect("turn/completed notification should include params"),
                        )?;
                        if payload.thread_id == thread.id && payload.turn.id == turn.id {
                            anyhow::bail!("turn completed before proof acceptance: {payload:#?}");
                        }
                    }
                    _ => {}
                }
            }
        }
    })
    .await??;
    assert!(
        pre_acceptance_payloads
            .iter()
            .all(|payload| !payload.contains(TERMINAL_TEXT)),
        "app-server published the final-looking candidate before proof acceptance: {pre_acceptance_payloads:#?}"
    );
    let proof_output = proof_result_request
        .get("input")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| {
            items.iter().find(|item| {
                item.get("type").and_then(serde_json::Value::as_str)
                    == Some("function_call_output")
                    && item.get("call_id").and_then(serde_json::Value::as_str)
                        == Some("proof-exec-call")
            })
        })
        .and_then(|item| item.get("output"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "missing string function_call_output for proof-exec-call in the second model request"
            )
        })?;
    assert!(
        proof_output
            .lines()
            .filter(|line| *line == ACCEPTED_PROOF_LINE)
            .count()
            == 1,
        "canonical direct-argv proof was not accepted; exact proof-exec-call output: {proof_output:?}"
    );
    assert_eq!(
        proof_output.lines().last(),
        Some(ACCEPTED_PROOF_LINE),
        "accepted-proof line must be the final output line; exact proof-exec-call output: {proof_output:?}"
    );
    assert!(
        !proof_result_request.to_string().contains(TERMINAL_TEXT),
        "terminal text was present in the second model request before proof acceptance"
    );

    let completed = timeout(READ_TIMEOUT, async {
        loop {
            let message = app_server.read_next_message().await?;
            let JSONRPCMessage::Notification(notification) = message else {
                continue;
            };
            match notification.method.as_str() {
                "item/started" => {
                    let payload: ItemStartedNotification = serde_json::from_value(
                        notification
                            .params
                            .expect("item/started notification should include params"),
                    )?;
                    if payload.thread_id == thread.id && payload.turn_id == turn.id {
                        started_items.push(payload.item);
                    }
                }
                "item/completed" => {
                    let payload: ItemCompletedNotification = serde_json::from_value(
                        notification
                            .params
                            .expect("item/completed notification should include params"),
                    )?;
                    if payload.thread_id == thread.id && payload.turn_id == turn.id {
                        completed_items.push(payload.item);
                    }
                }
                "turn/completed" => {
                    let payload: TurnCompletedNotification = serde_json::from_value(
                        notification
                            .params
                            .expect("turn/completed notification should include params"),
                    )?;
                    if payload.thread_id == thread.id && payload.turn.id == turn.id {
                        break Ok::<_, anyhow::Error>(payload);
                    }
                }
                _ => {}
            }
        }
    })
    .await??;

    let canonical_starts = started_items
        .iter()
        .filter(|item| matches!(item, ThreadItem::CommandExecution { .. }))
        .collect::<Vec<_>>();
    let canonical_completions = completed_items
        .iter()
        .filter(|item| matches!(item, ThreadItem::CommandExecution { .. }))
        .collect::<Vec<_>>();
    assert_eq!(canonical_starts.len(), 1, "canonical launch start count");
    assert_eq!(
        canonical_completions.len(),
        1,
        "canonical launch completion count"
    );
    let ThreadItem::CommandExecution {
        id: started_id,
        command: started_command,
        cwd: started_cwd,
        execution_id: started_execution_id,
        status: started_status,
        ..
    } = canonical_starts[0]
    else {
        unreachable!("canonical starts are filtered command executions")
    };
    let ThreadItem::CommandExecution {
        id: completed_id,
        command: completed_command,
        cwd: completed_cwd,
        execution_id: completed_execution_id,
        status: completed_status,
        exit_code,
        ..
    } = canonical_completions[0]
    else {
        unreachable!("canonical completions are filtered command executions")
    };
    assert_eq!(started_id, "proof-exec-call");
    assert_eq!(completed_id, started_id);
    assert_eq!(started_command, fixture.canonical_command());
    assert_eq!(completed_command, started_command);
    assert_eq!(
        started_cwd.as_str(),
        fixture.repo_path().to_string_lossy().as_ref()
    );
    assert_eq!(completed_cwd, started_cwd);
    assert_eq!(started_status, &CommandExecutionStatus::InProgress);
    assert_eq!(completed_status, &CommandExecutionStatus::Completed);
    assert_eq!(exit_code, &Some(0));
    assert!(
        started_execution_id.is_some(),
        "canonical launch must expose an execution_id"
    );
    assert_eq!(completed_execution_id, started_execution_id);
    let canonical_completion_index = completed_items
        .iter()
        .position(|item| {
            matches!(item, ThreadItem::CommandExecution { id, .. } if id == "proof-exec-call")
        })
        .expect("canonical command completion should be present");
    let completed_agent_messages = completed_items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let ThreadItem::AgentMessage { text, .. } = item else {
                return None;
            };
            Some((index, text.as_str()))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        completed_agent_messages.len(),
        1,
        "exactly one completed AgentMessage should be published: {completed_agent_messages:#?}"
    );
    let (terminal_publication_index, terminal_text) = completed_agent_messages[0];
    assert_eq!(terminal_text, TERMINAL_TEXT);
    assert!(
        terminal_publication_index > canonical_completion_index,
        "terminal text was published before the canonical command completed: {completed_items:#?}"
    );

    let requests = server
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("mock server did not record requests"))?;
    let responses_request_count = requests
        .iter()
        .filter(|request| request.method == "POST" && request.url.path().ends_with("/responses"))
        .count();
    assert_eq!(responses_request_count, 2, "exact model request count");

    assert!(fixture.canonical_runner_launched());
    assert_eq!(fixture.canonical_launch_count()?, 1);
    assert_eq!(completed.thread_id, thread.id);
    assert_eq!(completed.turn.id, turn.id);
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert!(completed.turn.error.is_none());
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_killed_real_cli_exec_preserves_exact_physical_credential() -> Result<()> {
    let fixture = BlockedCompletionProofFixture::new()?;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(Duration::from_secs(60))
                .set_body_string("data: {}\n\n"),
        )
        .mount(&server)
        .await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::new(),
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;
    let physical_credential_before =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            codex_home.path(),
            fixture.repo_path(),
        )
        .map_err(anyhow::Error::msg)?;

    let mut child = Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .env("CODEX_HOME", codex_home.path())
        .current_dir(fixture.repo_path())
        .args([
            "exec",
            "--skip-git-repo-check",
            "remain active until hard-killed",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    timeout(Duration::from_secs(30), async {
        loop {
            if server
                .received_requests()
                .await
                .is_some_and(|requests| !requests.is_empty())
            {
                return Ok::<(), anyhow::Error>(());
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("CLI exited before hard-kill readiness with {status}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    hard_kill_and_wait(&mut child).await?;

    let physical_credential_after =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            codex_home.path(),
            fixture.repo_path(),
        )
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        physical_credential_after, physical_credential_before,
        "hard-killed CLI changed the exact physical completion-proof credential"
    );
    Ok(())
}

#[cfg(windows)]
async fn hard_kill_and_wait(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    child.start_kill()?;
    child.wait().await
}
