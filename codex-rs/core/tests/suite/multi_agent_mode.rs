use anyhow::Context;
use anyhow::Result;
use codex_core::config::Config;
use codex_features::Feature;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MULTI_AGENT_MODE_OPEN_TAG;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

const NO_SPAWN_TEXT: &str = "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work.";
const PROACTIVE_TEXT: &str = "Proactive multi-agent delegation is active.";
const CUSTOM_MODE_HINT_TEXT: &str = "Use the configured delegation policy.";

fn add_ultra_reasoning(model_info: &mut ModelInfo) {
    model_info.supports_reasoning_summaries = true;
    model_info
        .supported_reasoning_levels
        .push(ReasoningEffortPreset {
            effort: ReasoningEffort::Ultra,
            description: "Ultra".to_string(),
        });
}

fn configure_multi_agent_v2(config: &mut Config) {
    // These tests exercise explicit effort and multi-agent mode mapping. Keep the
    // independent phase governor from replacing those requested effort values.
    config.reasoning_phase_efforts = None;
    config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test config should allow feature update");
}

// Configuring a custom mode hint also enables multi-agent V2 for the test.
fn configure_custom_mode_hint(config: &mut Config) {
    configure_multi_agent_v2(config);
    config.multi_agent_v2.multi_agent_mode_hint_text = Some(CUSTOM_MODE_HINT_TEXT.to_string());
}

fn configure_ultra(config: &mut Config) {
    configure_multi_agent_v2(config);
    config.model_reasoning_effort = Some(ReasoningEffort::Ultra);
}

fn developer_texts(input: &[Value]) -> Vec<&str> {
    input
        .iter()
        .filter(|item| item.get("role").and_then(Value::as_str) == Some("developer"))
        .filter_map(|item| item.get("content")?.as_array())
        .flatten()
        .filter_map(|content| content.get("text")?.as_str())
        .collect()
}

fn count_containing(texts: &[&str], target: &str) -> usize {
    texts.iter().filter(|text| text.contains(target)).count()
}

fn count_tagged(texts: &[&str], tag: &str) -> usize {
    texts
        .iter()
        .filter(|text| text.trim_start().starts_with(tag))
        .count()
}

async fn submit_turn(
    codex: &codex_core::CodexThread,
    prompt: &str,
    effort: Option<ReasoningEffort>,
) -> Result<()> {
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: ThreadSettingsOverrides {
                effort: effort.map(Some),
                ..Default::default()
            },
        })
        .await?;
    wait_for_event(codex, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ultra_reasoning_uses_max_and_requires_explicit_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(configure_ultra)
        .build(&server)
        .await?;

    submit_turn(&test.codex, "hello", /*effort*/ None).await?;

    let request = response.single_request();
    assert_eq!(
        request.body_json()["reasoning"]["effort"].as_str(),
        Some("max")
    );
    let input = request.input();
    let texts = developer_texts(&input);
    assert_eq!(
        (
            count_containing(&texts, NO_SPAWN_TEXT),
            count_containing(&texts, PROACTIVE_TEXT),
        ),
        (1, 0)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sol_high_xhigh_and_max_require_explicit_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=3)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("resp-{index}")),
                    ev_completed(&format!("resp-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(configure_multi_agent_v2)
        .build(&server)
        .await?;

    submit_turn(&test.codex, "high", Some(ReasoningEffort::High)).await?;
    submit_turn(&test.codex, "xhigh", Some(ReasoningEffort::XHigh)).await?;
    submit_turn(&test.codex, "max", Some(ReasoningEffort::Max)).await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    for (request, expected_effort) in requests.iter().zip(["high", "xhigh", "max"]) {
        assert_eq!(
            request.body_json()["reasoning"]["effort"].as_str(),
            Some(expected_effort)
        );
        let input = request.input();
        let texts = developer_texts(&input);
        assert_eq!(
            (
                count_containing(&texts, NO_SPAWN_TEXT),
                count_containing(&texts, PROACTIVE_TEXT),
            ),
            (1, 0)
        );
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_mode_hint_uses_custom_mode_across_reasoning_efforts() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=2)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("resp-{index}")),
                    ev_completed(&format!("resp-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(configure_custom_mode_hint)
        .build(&server)
        .await?;
    let rollout_path = test
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    submit_turn(&test.codex, "explicit", Some(ReasoningEffort::High)).await?;
    submit_turn(&test.codex, "proactive", Some(ReasoningEffort::Ultra)).await?;

    let requests = responses.requests();
    let first_input = requests[0].input();
    let first_texts = developer_texts(&first_input);
    let second_input = requests[1].input();
    let second_texts = developer_texts(&second_input);
    let instruction_counts = |texts: &[&str]| {
        (
            count_containing(texts, CUSTOM_MODE_HINT_TEXT),
            count_containing(texts, NO_SPAWN_TEXT),
            count_containing(texts, PROACTIVE_TEXT),
        )
    };
    assert_eq!(instruction_counts(&first_texts), (1, 0, 0));
    assert_eq!(instruction_counts(&second_texts), (1, 0, 0));
    let rollout_values = std::fs::read_to_string(rollout_path)?
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let recorded_modes = rollout_values
        .iter()
        .filter(|value| value.get("type").and_then(Value::as_str) == Some("turn_context"))
        .filter_map(|value| value.pointer("/payload/multi_agent_mode").cloned())
        .collect::<Vec<_>>();
    assert_eq!(
        recorded_modes,
        [
            json!({"custom": CUSTOM_MODE_HINT_TEXT}),
            json!({"custom": CUSTOM_MODE_HINT_TEXT}),
        ]
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn empty_configured_mode_hint_suppresses_builtin_text() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            configure_multi_agent_v2(config);
            config.multi_agent_v2.multi_agent_mode_hint_text = Some(String::new());
        })
        .build(&server)
        .await?;

    submit_turn(&test.codex, "hello", Some(ReasoningEffort::High)).await?;

    let input = response.single_request().input();
    let texts = developer_texts(&input);
    assert_eq!(
        (
            count_tagged(&texts, MULTI_AGENT_MODE_OPEN_TAG),
            count_containing(&texts, NO_SPAWN_TEXT),
            count_containing(&texts, PROACTIVE_TEXT),
        ),
        (1, 0, 0)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn leaving_ultra_after_cold_resume_emits_explicit_mode() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        (1..=2)
            .map(|index| {
                sse(vec![
                    ev_response_created(&format!("resp-{index}")),
                    ev_completed(&format!("resp-{index}")),
                ])
            })
            .collect(),
    )
    .await;
    let initial = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(configure_ultra)
        .build(&server)
        .await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .session_configured
        .rollout_path
        .clone()
        .expect("rollout path");

    submit_turn(&initial.codex, "before resume", /*effort*/ None).await?;
    drop(initial);

    let mut resume_builder = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(configure_ultra);
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    submit_turn(&resumed.codex, "after resume", Some(ReasoningEffort::High)).await?;

    let requests = responses.requests();
    assert_eq!(
        (
            requests[0].body_json()["reasoning"]["effort"]
                .as_str()
                .map(str::to_string),
            requests[1].body_json()["reasoning"]["effort"]
                .as_str()
                .map(str::to_string),
        ),
        (Some("max".to_string()), Some("high".to_string()))
    );
    let resumed_input = requests[1].input();
    let texts = developer_texts(&resumed_input);
    assert_eq!(
        (
            count_tagged(&texts, MULTI_AGENT_MODE_OPEN_TAG),
            count_containing(&texts, NO_SPAWN_TEXT),
            count_containing(&texts, PROACTIVE_TEXT),
        ),
        (1, 1, 0)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ultra_on_multi_agent_v1_uses_max_without_mode_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", add_ultra_reasoning)
        .with_config(|config| {
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.reasoning_phase_efforts = None;
            config.model_reasoning_effort = Some(ReasoningEffort::Ultra);
        })
        .build(&server)
        .await?;

    submit_turn(&test.codex, "hello", /*effort*/ None).await?;

    let request = response.single_request();
    assert_eq!(
        request.body_json()["reasoning"]["effort"].as_str(),
        Some("max")
    );
    let input = request.input();
    let texts = developer_texts(&input);
    assert_eq!(count_tagged(&texts, MULTI_AGENT_MODE_OPEN_TAG), 0);

    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_isolated_spawn_reaps_checkout_descendant_and_removes_worktree() -> Result<()> {
    assert_interrupted_isolated_spawn(None).await
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_isolated_spawn_namespaced_reaps_checkout_descendant_and_removes_worktree()
-> Result<()> {
    assert_interrupted_isolated_spawn(Some("agents")).await
}

#[cfg(windows)]
async fn assert_interrupted_isolated_spawn(namespace: Option<&str>) -> Result<()> {
    use std::process::Command;
    use std::time::Duration;

    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let args = json!({
        "task_name": "cancelled_worker", "agent_type": "worker",
        "assignment": {
            "objective": "Inspect the tracked file in an isolated workspace",
            "acceptance_criteria": [{"id": "report", "text": "Report the file contents"}],
            "write_scope": [{"path": "tracked.txt"}], "stop_condition": "Stop after reporting",
            "required_evidence": ["Report the file contents"],
            "workspace_strategy": "isolated"
        }
    });
    let response = mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("isolated-spawn"),
            match namespace {
                Some(namespace) => core_test_support::responses::ev_function_call_with_namespace(
                    "isolated-call",
                    namespace,
                    "spawn_agent",
                    &args.to_string(),
                ),
                None => core_test_support::responses::ev_function_call(
                    "isolated-call",
                    "spawn_agent",
                    &args.to_string(),
                ),
            },
            ev_completed("isolated-spawn"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config({
            let namespace = namespace.map(str::to_string);
            move |config| {
                configure_multi_agent_v2(config);
                config.multi_agent_v2.tool_namespace = namespace;
            }
        })
        .build(&server)
        .await?;
    for args in [
        vec!["init"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.invalid"],
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(test.cwd_path())
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    std::fs::write(test.cwd_path().join("tracked.txt"), "committed input\n")?;
    for args in [vec!["add", "tracked.txt"], vec!["commit", "-m", "fixture"]] {
        let output = Command::new("git")
            .arg("-C")
            .arg(test.cwd_path())
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let markers = tempfile::tempdir()?;
    let marker = markers.path().join("checkout-child.txt");
    let script = markers.path().join("checkout-child.ps1");
    std::fs::write(
        &script,
        format!(
            "[IO.File]::WriteAllLines('{}', @([string]$PID, (Get-Location).Path))\nStart-Sleep -Seconds 60\n",
            marker.to_string_lossy().replace('\'', "''")
        ),
    )?;
    std::fs::write(
        test.cwd_path().join(".git/hooks/post-checkout"),
        format!(
            "#!/bin/sh\nexec powershell.exe -NoProfile -ExecutionPolicy Bypass -File '{}'\n",
            script
                .to_string_lossy()
                .replace('\\', "/")
                .replace('\'', "'\\''")
        ),
    )?;
    let threads_before = test.thread_manager.list_thread_created_ids().await;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "Use a subagent in an isolated worktree to inspect tracked.txt".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let recorded = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&marker).await
                && text.lines().count() == 2
            {
                break Ok::<String, anyhow::Error>(text);
            }
            if let Some(output) = response.requests().iter().find_map(|request| request.function_call_output_text("isolated-call")) {
                anyhow::bail!("registered spawn returned before checkout hook started: {output}");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .with_context(|| format!("waiting for isolated checkout hook: namespace={namespace:?}, model_requests={}, created_threads={threads_before:?}", response.requests().len()))??;
    let mut lines = recorded.lines();
    let pid: u32 = lines.next().expect("child pid").parse()?;
    // Keep the deliberately stalled fixture bounded even if this regression fails.
    struct ChildCleanup(u32);
    impl Drop for ChildCleanup {
        fn drop(&mut self) {
            let _ = Command::new("taskkill.exe")
                .args(["/PID", &self.0.to_string(), "/T", "/F"])
                .output();
        }
    }
    let _child_cleanup = ChildCleanup(pid);
    let worktree = std::path::PathBuf::from(lines.next().expect("child working directory"));
    assert!(worktree.join("tracked.txt").exists());
    assert_ne!(worktree, test.cwd_path());
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_))
    })
    .await;
    assert!(
        !worktree.exists(),
        "aborted turn published before isolated worktree rollback completed"
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let process = tokio::process::Command::new("powershell.exe").kill_on_drop(true).args(["-NoProfile", "-Command", &format!(
                "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }} else {{ exit 0 }}"
            )]).output().await?;
            let listing = tokio::process::Command::new("git").kill_on_drop(true).arg("-C").arg(test.cwd_path()).args(["worktree", "list", "--porcelain"]).output().await?;
            if process.status.success() && !worktree.exists()
                && listing.status.success()
                && String::from_utf8_lossy(&listing.stdout).lines().filter(|line| line.starts_with("worktree ")).count() == 1 { break; }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok::<(), anyhow::Error>(())
    }).await??;
    assert_eq!(
        test.thread_manager.list_thread_created_ids().await,
        threads_before
    );
    assert_eq!(response.requests().len(), 1);
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_spawn_task_capsule_delivers_normalized_handles_and_file_revision() -> Result<()> {
    use core_test_support::responses::ev_function_call;
    use core_test_support::responses::mount_sse_once_match;
    use std::time::Duration;

    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    const PROMPT: &str = "Please spawn a subagent to inspect target.txt using a task capsule.";
    const OBJECTIVE: &str = "Inspect the capsule target file";
    let args = json!({
        "task_name": "capsule_worker", "agent_type": "worker", "fork_turns": "none",
        "assignment": {
            "objective": OBJECTIVE,
            "acceptance_criteria": [{"id": "report", "text": "Report the source bytes"}],
            "read_scope": [{"path": "target.txt"}], "write_scope": [{"path": "target.txt"}],
            "stop_condition": "Stop after reporting",
            "required_evidence": ["Report the source bytes"],
            "relevant_handles": [
                {"kind": "file", "path": "target.txt"},
                {"kind": "symbol", "path": "target.txt", "symbol": "  capsule_target  "}
            ]
        }
    });
    let parent_requests = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| String::from_utf8_lossy(&request.body).contains(PROMPT),
        sse(vec![
            ev_response_created("capsule-parent"),
            ev_function_call("capsule-spawn", "spawn_agent", &args.to_string()),
            ev_completed("capsule-parent"),
        ]),
    )
    .await;
    let child_requests = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains("<task_capsule_v1>") && body.contains(OBJECTIVE)
        },
        sse(vec![
            ev_response_created("capsule-child"),
            ev_completed("capsule-child"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            String::from_utf8_lossy(&request.body).contains("capsule-spawn")
        },
        sse(vec![
            ev_response_created("capsule-parent-done"),
            ev_completed("capsule-parent-done"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config(|config| {
            configure_multi_agent_v2(config);
            config.multi_agent_v2.tool_namespace = None;
        })
        .build(&server)
        .await?;
    std::fs::write(test.cwd_path().join("target.txt"), b"capsule file bytes\n")?;
    test.submit_turn(PROMPT).await?;
    let capsule = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            for request in child_requests.requests() {
                for item in request.input() {
                    if item["role"] != "user" {
                        continue;
                    }
                    let Some(content) = item["content"].as_array() else {
                        continue;
                    };
                    for part in content {
                        if let Some(payload) = part["text"]
                            .as_str()
                            .and_then(|text| text.strip_prefix("<task_capsule_v1>"))
                            .and_then(|text| text.strip_suffix("</task_capsule_v1>"))
                        {
                            let capsule: Value = serde_json::from_str(payload)?;
                            if capsule["objective"] == OBJECTIVE {
                                return Ok::<Value, anyhow::Error>(capsule);
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("waiting for normalized child task capsule: parent_requests={}, observed_spawn_outputs={:?}, candidate_child_requests={}", parent_requests.requests().len(), child_requests.requests().iter().filter_map(|request| request.function_call_output_text("capsule-spawn")).collect::<Vec<_>>(), child_requests.requests().len()))??;
    assert_eq!(capsule["schema_version"], 1);
    assert_eq!(capsule["objective"], OBJECTIVE);
    assert_eq!(
        capsule["requirements"],
        json!([{"id": "report", "text": "Report the source bytes"}])
    );
    assert_eq!(
        capsule["relevant_handles"],
        json!([
            {"kind": "file", "path": "target.txt", "existed": true, "content_hash": "30bcd71c556c7f15b1687bca82588f0406d2075774e5514cd95644414fbdfe99"},
            {"kind": "symbol", "path": "target.txt", "symbol": "capsule_target", "existed": true, "content_hash": "30bcd71c556c7f15b1687bca82588f0406d2075774e5514cd95644414fbdfe99"}
        ])
    );
    assert_eq!(
        std::fs::read(test.cwd_path().join("target.txt"))?,
        b"capsule file bytes\n"
    );
    assert_eq!(test.thread_manager.list_thread_ids().await.len(), 2);
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_wait_agent_invalid_cursor_completes_started_item() -> Result<()> {
    assert_registered_wait_item_terminal(None, json!({"cursor": "not-a-wake-event"}), false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_wait_agent_cancellation_completes_started_item() -> Result<()> {
    assert_registered_wait_item_terminal(None, json!({}), true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_wait_agent_timeout_completes_started_item() -> Result<()> {
    assert_registered_wait_item_terminal(None, json!({"timeout_ms": 0}), false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_wait_agent_namespaced_cancellation_completes_started_item() -> Result<()> {
    assert_registered_wait_item_terminal(Some("agents"), json!({}), true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registered_wait_agent_deadline_bounds_stalled_maintenance() -> Result<()> {
    assert_registered_wait_item_terminal_with_stalled_store(
        None,
        json!({"timeout_ms": 50}),
        false,
        true,
    )
    .await
}

async fn assert_registered_wait_item_terminal(
    namespace: Option<&str>,
    wait_args: Value,
    cancel: bool,
) -> Result<()> {
    assert_registered_wait_item_terminal_with_stalled_store(namespace, wait_args, cancel, false)
        .await
}

async fn assert_registered_wait_item_terminal_with_stalled_store(
    namespace: Option<&str>,
    wait_args: Value,
    cancel: bool,
    stall_maintenance: bool,
) -> Result<()> {
    use codex_protocol::items::CollabAgentTool;
    use codex_protocol::items::CollabAgentToolCallStatus;
    use codex_protocol::items::TurnItem;
    use core_test_support::responses;
    use std::time::Duration;

    skip_if_no_network!(Ok(()));
    const PROMPT: &str = "Spawn a subagent, then wait for its work.";
    const PRIME_PROMPT: &str =
        "Drain the child startup observations once before the lifecycle test.";
    const TEST_PROMPT: &str = "Run the selected wait lifecycle check now.";
    const PRIME_ID: &str = "wait-lifecycle-primer";
    const CHILD_TASK: &str = "Stay active while the parent verifies wait event ownership.";
    const SPAWN_ID: &str = "wait-lifecycle-spawn";
    const WAIT_ID: &str = "wait-lifecycle-call";
    let server = start_mock_server().await;
    let call = |id: &str, name: &str, args: String| match namespace {
        Some(namespace) => responses::ev_function_call_with_namespace(id, namespace, name, &args),
        None => responses::ev_function_call(id, name, &args),
    };
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| String::from_utf8_lossy(&request.body).contains(PROMPT),
        sse(vec![
            ev_response_created("wait-lifecycle-parent"),
            call(
                SPAWN_ID,
                "spawn_agent",
                json!({"message": CHILD_TASK, "task_name": "wait_lifecycle_child"}).to_string(),
            ),
            ev_completed("wait-lifecycle-parent"),
        ]),
    )
    .await;
    let child_response = responses::mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains(CHILD_TASK) && !body.contains(SPAWN_ID)
        },
        responses::sse_response(sse(vec![
            ev_response_created("wait-lifecycle-child"),
            ev_completed("wait-lifecycle-child"),
        ]))
        .set_delay(Duration::from_secs(30)),
    )
    .await;
    let spawn_response = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains(SPAWN_ID) && !body.contains(PRIME_PROMPT) && !body.contains(TEST_PROMPT)
        },
        sse(vec![
            ev_response_created("wait-lifecycle-spawned"),
            ev_completed("wait-lifecycle-spawned"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains(PRIME_PROMPT) && !body.contains(PRIME_ID) && !body.contains(TEST_PROMPT)
        },
        sse(vec![
            ev_response_created("wait-lifecycle-prime"),
            call(PRIME_ID, "wait_agent", json!({"timeout_ms": 0}).to_string()),
            ev_completed("wait-lifecycle-prime"),
        ]),
    )
    .await;
    let primed_response = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains(PRIME_ID) && !body.contains(TEST_PROMPT)
        },
        sse(vec![
            ev_response_created("wait-lifecycle-primed"),
            ev_completed("wait-lifecycle-primed"),
        ]),
    )
    .await;
    responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            let body = String::from_utf8_lossy(&request.body);
            body.contains(TEST_PROMPT) && !body.contains(WAIT_ID)
        },
        sse(vec![
            ev_response_created("wait-lifecycle-wait"),
            call(WAIT_ID, "wait_agent", wait_args.to_string()),
            ev_completed("wait-lifecycle-wait"),
        ]),
    )
    .await;
    let completed_response = if cancel {
        None
    } else {
        Some(
            responses::mount_sse_once_match(
                &server,
                |request: &wiremock::Request| {
                    String::from_utf8_lossy(&request.body).contains(WAIT_ID)
                },
                sse(vec![
                    ev_response_created("wait-lifecycle-done"),
                    ev_completed("wait-lifecycle-done"),
                ]),
            )
            .await,
        )
    };
    let test = test_codex()
        .with_model("gpt-5.6-sol")
        .with_config({
            let namespace = namespace.map(str::to_string);
            move |config| {
                configure_multi_agent_v2(config);
                config.multi_agent_v2.tool_namespace = namespace;
                config.multi_agent_v2.min_wait_timeout_ms = 0;
                if stall_maintenance {
                    config.multi_agent_v2.default_wait_timeout_ms = 5;
                }
                config
                    .features
                    .disable(Feature::EnableRequestCompression)
                    .expect("HTTP fixture uses plaintext request matchers");
            }
        })
        .build(&server)
        .await?;
    test.submit_turn(PROMPT).await?;
    let spawn_output = spawn_response
        .function_call_output_text(SPAWN_ID)
        .expect("normal registered spawn returned a model-visible result");
    let spawn_result: Value = serde_json::from_str(&spawn_output)?;
    assert!(spawn_result.get("error").is_none(), "{spawn_output}");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !child_response.requests().iter().any(|request| {
            let body = request.body_json().to_string();
            body.contains(CHILD_TASK) && !body.contains(SPAWN_ID)
        }) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("child entered its delayed normal model request before priming")?;
    // A normal registered wait drains the Accepted/Starting wake backlog before
    // testing timeout or cancellation in a separate user turn.
    test.submit_turn(PRIME_PROMPT).await?;
    let primed_output = primed_response
        .function_call_output_text(PRIME_ID)
        .expect("normal registered primer returned a model-visible result");
    let primed: Value = serde_json::from_str(&primed_output)?;
    assert_eq!(
        primed["message"],
        "Durable typed-task progress is available."
    );
    assert_eq!(primed["timed_out"], false);
    assert!(
        primed["typed_deltas"]
            .as_array()
            .is_some_and(|deltas| !deltas.is_empty())
    );
    assert!(
        primed["cursor"]
            .as_str()
            .is_some_and(|cursor| !cursor.is_empty())
    );
    // WAL readers remain usable while an independent writer holds recovery's
    // transaction boundary. This stalls the real maintenance operation only.
    let mut stalled_store = if stall_maintenance {
        use sqlx::Connection;
        let database = test
            .config
            .sqlite_home
            .join("agent-task-coordination")
            .join("agent_tasks.sqlite");
        assert!(
            database.is_file(),
            "normal spawn initialized the coordination store"
        );
        let mut connection = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(database),
        )
        .await?;
        let stale = serde_json::to_string(&(chrono::Utc::now() - chrono::Duration::minutes(10)))?;
        let updated = sqlx::query(
            "UPDATE workspace_actors SET last_progress_at = ? WHERE assignment_id = ? AND state <> 'terminal'",
        )
        .bind(stale)
        .bind(
            spawn_result["assignment_id"]
                .as_str()
                .expect("normal spawn returned its durable assignment identity"),
        )
        .execute(&mut connection)
        .await?;
        assert_eq!(
            updated.rows_affected(),
            1,
            "exactly the spawned active child is stale"
        );
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut connection)
            .await?;
        Some(connection)
    } else {
        None
    };
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: TEST_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let mut history = codex_app_server_protocol::ThreadHistoryBuilder::new();
    let mut statuses = Vec::new();
    let mut wait_turn_id = None;
    tokio::time::timeout(Duration::from_secs(if stall_maintenance { 2 } else { 20 }), async {
        loop {
            let event = test.codex.next_event().await?;
            history.handle_event(&event.msg);
            match event.msg {
                EventMsg::ItemStarted(started) => {
                    if let TurnItem::CollabAgentToolCall(item) = started.item && item.id == WAIT_ID {
                        assert_eq!(item.tool, CollabAgentTool::Wait);
                        assert_eq!(item.status, CollabAgentToolCallStatus::InProgress);
                        assert_eq!(started.thread_id, test.session_configured.thread_id);
                        assert!(wait_turn_id.replace(started.turn_id).is_none(), "duplicate wait start");
                        statuses.push(item.status);
                        if cancel {
                            test.codex.submit(Op::Interrupt).await?;
                        }
                    }
                }
                EventMsg::ItemCompleted(completed) => {
                    if let TurnItem::CollabAgentToolCall(item) = completed.item && item.id == WAIT_ID {
                        assert_eq!(item.tool, CollabAgentTool::Wait);
                        assert_eq!(Some(completed.turn_id), wait_turn_id);
                        assert_eq!(completed.thread_id, test.session_configured.thread_id);
                        statuses.push(item.status);
                    }
                }
                EventMsg::TurnAborted(_) => {
                    assert!(cancel, "noncancelled wait unexpectedly aborted");
                    break;
                }
                EventMsg::TurnComplete(_) => {
                    assert!(!cancel, "cancelled wait completed its turn before interruption");
                    break;
                }
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    }).await.with_context(|| format!("wait event lifecycle namespace={namespace:?}, statuses={statuses:?}, spawn_result={:?}", spawn_response.function_call_output_text(SPAWN_ID)))??;
    let expected_status = if cancel || wait_args.get("cursor").is_some() {
        CollabAgentToolCallStatus::Failed
    } else {
        CollabAgentToolCallStatus::Completed
    };
    assert_eq!(
        statuses,
        vec![CollabAgentToolCallStatus::InProgress, expected_status],
        "terminal wait item must precede turn termination exactly once"
    );
    let turns = history.finish();
    let observed_turn = turns
        .iter()
        .find(|turn| Some(&turn.id) == wait_turn_id.as_ref())
        .expect("history consumer retained the tested turn");
    assert_eq!(
        observed_turn.status,
        if cancel {
            codex_app_server_protocol::TurnStatus::Interrupted
        } else {
            codex_app_server_protocol::TurnStatus::Completed
        }
    );
    let observed_waits =
        observed_turn
            .items
            .iter()
            .filter_map(|item| match item {
                codex_app_server_protocol::ThreadItem::CollabAgentToolCall {
                    id, status, ..
                } if id == WAIT_ID => Some(status),
                _ => None,
            })
            .collect::<Vec<_>>();
    let expected_history_status = if cancel || wait_args.get("cursor").is_some() {
        codex_app_server_protocol::CollabAgentToolCallStatus::Failed
    } else {
        codex_app_server_protocol::CollabAgentToolCallStatus::Completed
    };
    assert_eq!(
        observed_waits,
        vec![&expected_history_status],
        "the history consumer must close the wait item independently of turn status"
    );
    if let Some(completed_response) = completed_response {
        let output = completed_response
            .function_call_output_text(WAIT_ID)
            .expect("actual model-visible wait output");
        if wait_args.get("cursor").is_some() {
            assert!(output.contains("wait_agent cursor is invalid"), "{output}");
        } else {
            let result: Value = serde_json::from_str(&output)?;
            assert_eq!(result["message"], "Wait timed out.");
            assert_eq!(result["timed_out"], true);
        }
    }
    if let Some(connection) = stalled_store.as_mut() {
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM attempts WHERE json_extract(state, '$') = 'active'",
        )
        .fetch_one(&mut *connection)
        .await?;
        assert_eq!(
            active, 1,
            "deadline must not abandon the live child while recovery is stalled"
        );
        sqlx::query("ROLLBACK").execute(&mut *connection).await?;
        // Acquiring another writer transaction after release also observes cleanup
        // of the stalled no-op writer before checking the durable child state.
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let state: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM attempts WHERE json_extract(state, '$') = 'active'), (SELECT COUNT(*) FROM receipts)"
        ).fetch_one(&mut *connection).await?;
        assert_eq!(state, (1, 0), "released maintenance must not later abandon or seal the child");
        sqlx::query("ROLLBACK").execute(&mut *connection).await?;
    }
    drop(stalled_store);
    for thread_id in test.thread_manager.list_thread_ids().await {
        if thread_id != test.session_configured.thread_id {
            let child = test.thread_manager.get_thread(thread_id).await?;
            child.submit(Op::Shutdown).await?;
            wait_for_event(&child, |event| matches!(event, EventMsg::ShutdownComplete)).await;
        }
    }
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    Ok(())
}
