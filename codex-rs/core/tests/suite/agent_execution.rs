use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::request_has_last_message_input_text;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;

const FIRST_PROMPT: &str = "spawn the first agent";
const FIRST_TASK: &str = "spawn the second agent";
const SECOND_TASK: &str = "second worker task";
const MULTI_AGENT_V2_NAMESPACE: &str = "agents";
const TASK_CAPSULE_OPEN_TAG: &str = "<task_capsule_v1>";
const TASK_CAPSULE_CLOSE_TAG: &str = "</task_capsule_v1>";

fn has_function_call_output(request: &wiremock::Request, call_id: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(|body| {
        body.get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(serde_json::Value::as_str)
                        == Some("function_call_output")
                        && item.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
                })
            })
    })
}

fn has_task_capsule_objective(request: &wiremock::Request, objective: &str) -> bool {
    serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(|body| {
        body.get("input")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("type").and_then(serde_json::Value::as_str) == Some("message")
                        && item.get("role").and_then(serde_json::Value::as_str) == Some("user")
                        && item
                            .get("content")
                            .and_then(serde_json::Value::as_array)
                            .is_some_and(|content| {
                                content.iter().any(|part| {
                                    part.get("type").and_then(serde_json::Value::as_str)
                                        == Some("input_text")
                                        && part
                                            .get("text")
                                            .and_then(serde_json::Value::as_str)
                                            .and_then(|text| {
                                                text.strip_prefix(TASK_CAPSULE_OPEN_TAG).and_then(
                                                    |text| {
                                                        text.strip_suffix(TASK_CAPSULE_CLOSE_TAG)
                                                    },
                                                )
                                            })
                                            .and_then(|payload| {
                                                serde_json::from_str::<serde_json::Value>(payload)
                                                    .ok()
                                            })
                                            .and_then(|capsule| {
                                                capsule
                                                    .get("objective")
                                                    .and_then(serde_json::Value::as_str)
                                                    .map(|value| value == objective)
                                            })
                                            == Some(true)
                                })
                            })
                })
            })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_nested_spawn_checks_shared_active_execution_capacity() -> Result<()> {
    let server = start_mock_server().await;
    let first_args = serde_json::to_string(&json!({
        "message": FIRST_TASK,
        "task_name": "first",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_last_message_input_text(request, "user", FIRST_PROMPT)
        },
        sse(vec![
            ev_response_created("first-response"),
            ev_function_call_with_namespace(
                "first-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &first_args,
            ),
            ev_completed("first-response"),
        ]),
    )
    .await;
    let second_args = serde_json::to_string(&json!({
        "message": SECOND_TASK,
        "task_name": "second",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            has_task_capsule_objective(request, FIRST_TASK)
                && !has_function_call_output(request, "first-call")
        },
        sse(vec![
            ev_response_created("first-worker-response"),
            ev_function_call_with_namespace(
                "second-call",
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &second_args,
            ),
            ev_completed("first-worker-response"),
        ]),
    )
    .await;
    let second_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "second-call"),
        sse(vec![
            ev_response_created("second-followup-response"),
            ev_assistant_message("second-followup-message", "blocked"),
            ev_completed("second-followup-response"),
        ]),
    )
    .await;
    let first_followup = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            has_function_call_output(request, "first-call")
                && !has_function_call_output(request, "hold-parent")
        },
        sse(vec![
            ev_response_created("first-followup-response"),
            ev_function_call("hold-parent", "request_user_input", &json!({
                "questions": [{
                    "id": "release", "header": "Continue", "question": "Release the parent?",
                    "options": [
                        {"label": "Yes", "description": "Finish this turn."},
                        {"label": "No", "description": "Keep waiting."}
                    ]
                }]
            }).to_string()),
            ev_completed("first-followup-response"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_function_call_output(request, "hold-parent"),
        sse(vec![
            ev_response_created("parent-finish"),
            ev_assistant_message("first-followup-message", "spawned"),
            ev_completed("parent-finish"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_model("koffing").with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        config.multi_agent_v2.max_concurrent_threads_per_session = 2;
        config.features.enable(Feature::DefaultModeRequestUserInput)
            .expect("enable the parent lifetime barrier");
    });
    let test = builder.build(&server).await?;
    test.codex.submit(Op::UserInput {
        items: vec![UserInput::Text {
            text: FIRST_PROMPT.to_string(), text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: Default::default(),
    }).await?;
    // Keep the root's execution slot occupied until the child's admission has
    // been observed. An immediate parent completion races the nested spawn.
    let question = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::RequestUserInput(question) => Some(question.clone()),
        _ => None,
    }).await;
    assert_eq!(question.call_id, "hold-parent");

    let second_output = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(output) = second_followup.function_call_output_text("second-call") {
                return output;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let second_output = match second_output {
        Ok(output) => output,
        Err(error) => {
            let request_summary = server
                .received_requests()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|request| {
                    let body = String::from_utf8_lossy(&request.body);
                    (
                        body.contains(FIRST_PROMPT),
                        body.contains(FIRST_TASK),
                        body.contains(SECOND_TASK),
                        body.contains("first-call"),
                        body.contains("second-call"),
                    )
                })
                .collect::<Vec<_>>();
            anyhow::bail!(
                "{error}; first spawn output: {:?}; requests (first prompt, first task, second task, first call, second call): {request_summary:?}",
                first_followup.function_call_output_text("first-call")
            );
        }
    };
    assert_eq!(
        second_output,
        "collab spawn failed: agent thread limit reached"
    );
    assert_eq!(test.thread_manager.list_thread_ids().await.len(), 2);

    test.codex.submit(Op::UserInputAnswer {
        id: question.turn_id.clone(),
        call_id: Some(question.call_id),
        response: RequestUserInputResponse {
            disposition: None,
            answers: [("release".to_string(), RequestUserInputAnswer {
                answers: vec!["Yes".to_string()],
            })].into_iter().collect(),
            interrupted: false,
        },
    }).await?;
    wait_for_event(&test.codex, |event| match event {
        EventMsg::TurnComplete(completed) => {
            assert_eq!(completed.turn_id, question.turn_id);
            assert_eq!(completed.error, None);
            assert_eq!(completed.last_agent_message.as_deref(), Some("spawned"));
            true
        }
        _ => false,
    }).await;

    Ok(())
}

/// Matched offline benchmark: only provider responses are scripted. The measured
/// interval includes admission, four real children, result collection, and the
/// parent's final response. Cleanup is checked and reported separately.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_agent_end_to_end_batch_benchmark() -> Result<()> {
    run_multi_agent_batch_benchmark(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_agent_end_to_end_wait_overlap_benchmark() -> Result<()> {
    run_multi_agent_batch_benchmark(true).await
}

async fn run_multi_agent_batch_benchmark(overlapping_wait: bool) -> Result<()> {
    for trial in 0..3 {
        let server = start_mock_server().await;
        let wait_prefix = if overlapping_wait {
            r#"
const pendingWait = resolve_tool("agents.wait_agent")({timeout_ms: 1000});
await new Promise(resolve => setTimeout(resolve, 50));
"#
        } else {
            ""
        };
        let wait_suffix = if overlapping_wait {
            r#"
const wake = await pendingWait;
text(wake.timed_out ? "overlap_wait_timed_out" : "overlap_wait_woke");
"#
        } else {
            ""
        };
        let batch_script = format!("{wait_prefix}{}{wait_suffix}", r#"
const spawn = resolve_tool("agents.spawn_agent");
const outcomes = await Promise.allSettled([0, 1, 2, 3].map(i =>
    spawn({task_name: "batch_" + i, message: "Return batch result " + i})));
if (outcomes.some(x => x.status !== "fulfilled" || !x.value.assignment_id))
    throw Error(JSON.stringify(outcomes));
const names = outcomes.map(x => x.value.task_name);
const list = resolve_tool("agents.list_agents");
const wait = resolve_tool("agents.wait_agent");
for (let attempt = 0; attempt < 256; attempt++) {
    const current = await list({});
    const workers = names.map(name => current.agents.find(x => x.agent_name === name));
    if (workers.every((x, i) => x && typeof x.agent_status === "object"
        && x.agent_status.completed?.includes("batch-result-" + i))) {
        text({batch_complete: true, workers});
        break;
    }
    if (attempt === 255) throw Error(JSON.stringify(current));
    await wait({timeout_ms: 1000});
    // Mailbox wakes remain pending until the next model request. A bounded
    // fixture backoff prevents repeatedly reading that same queued wake.
    await new Promise(resolve => setTimeout(resolve, 10));
}
"#);
        mount_sse_once_match(
            &server,
            |request: &wiremock::Request| {
                request_has_last_message_input_text(request, "user", "Run the four independent agents")
            },
            sse(vec![
                ev_response_created("batch-start"),
                ev_custom_tool_call("batch", "exec", &batch_script),
                ev_completed("batch-start"),
            ]),
        ).await;
        for worker in 0..4 {
            let objective = format!("Return batch result {worker}");
            let response = sse(vec![
                ev_response_created(&format!("child-{worker}")),
                ev_assistant_message(&format!("result-{worker}"), &format!("batch-result-{worker}")),
                ev_completed(&format!("child-{worker}")),
            ]);
            mount_sse_once_match(
                &server,
                move |request: &wiremock::Request| has_task_capsule_objective(request, &objective),
                response,
            ).await;
        }
        let finish = mount_sse_once_match(
            &server,
            |request: &wiremock::Request| {
                serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(|body| {
                    body["input"].as_array().is_some_and(|items| items.iter().any(|item| {
                        item["type"] == "custom_tool_call_output" && item["call_id"] == "batch"
                    }))
                })
            },
            sse(vec![
                ev_response_created("batch-finish"),
                ev_assistant_message("batch-answer", "All four results collected"),
                ev_completed("batch-finish"),
            ]),
        ).await;
        let mut builder = test_codex()
            .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = true)
            .with_config(|config| {
                for feature in [Feature::Collab, Feature::MultiAgentV2, Feature::CodeMode, Feature::CodeModeOnly] {
                    config.features.enable(feature).expect("enable batch fixture");
                }
                config.multi_agent_v2.max_concurrent_threads_per_session = 5;
                config.multi_agent_v2.min_wait_timeout_ms = 0;
            });
        let test = builder.build(&server).await?;
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(45),
            test.submit_turn_and_capture_completion("Run the four independent agents"),
        ).await;
        let completion_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut terminal_results = std::collections::BTreeMap::new();
        for id in test.thread_manager.list_thread_ids().await {
            let thread = test.thread_manager.get_thread(id).await?;
            if let Some(path) = thread.config_snapshot().await.session_source.get_agent_path() {
                terminal_results.insert(path.to_string(), thread.agent_status().await);
            }
        }
        let cleanup_started = std::time::Instant::now();
        let shutdown = test.thread_manager.shutdown_all_threads_bounded(Duration::from_secs(5)).await;
        let cleanup_ms = cleanup_started.elapsed().as_secs_f64() * 1000.0;
        let completed = outcome.context("batch timed out")??;
        assert_eq!(completed.error, None);
        assert_eq!(completed.last_agent_message.as_deref(), Some("All four results collected"));
        let request = finish.single_request();
        let (_, success) = request.custom_tool_call_output_content_and_success("batch")
            .expect("batch output");
        assert_ne!(success, Some(false), "the batch cell must succeed");
        let output = request.custom_tool_call_output("batch").to_string();
        anyhow::ensure!(shutdown.timed_out.is_empty() && shutdown.submit_failed.is_empty(), "{shutdown:?}");
        anyhow::ensure!(shutdown.completed.len() == 5, "lost or duplicated child: {shutdown:?}; batch output: {output}");
        let requests = server.received_requests().await.unwrap_or_default();
        for worker in 0..4 {
            let objective = format!("Return batch result {worker}");
            let count = requests.iter().filter(|request| has_task_capsule_objective(request, &objective)).count();
            anyhow::ensure!(count == 1, "child {worker} must execute exactly once, got {count}; {output}");
        }
        anyhow::ensure!(output.contains("batch_complete"), "collection failed: {output}");
        if overlapping_wait {
            anyhow::ensure!(output.contains("overlap_wait_"), "missing wait outcome: {output}");
        }
        for worker in 0..4 {
            anyhow::ensure!(output.contains(&format!("batch-result-{worker}")), "lost result: {output}");
            assert_eq!(
                terminal_results.get(&format!("/root/batch_{worker}")),
                Some(&AgentStatus::Completed(Some(format!(
                    "Agent-reported result (behavior unverified): batch-result-{worker}"
                )))),
            );
        }
        let model_requests = requests.iter().filter(|request| request.url.path().ends_with("/responses")).count();
        anyhow::ensure!(model_requests == 6, "unexpected coordination cycles: {model_requests}");
        eprintln!("multi_agent_batch_benchmark {}", json!({
            "trial": trial, "agents": 4, "model_requests": model_requests,
            "overlapping_wait": overlapping_wait,
            "overlap_wait_timed_out": output.contains("overlap_wait_timed_out"),
            "parent_completion_ms": completion_ms, "cleanup_ms": cleanup_ms,
            "total_ms": started.elapsed().as_secs_f64() * 1000.0,
            "provider": "scripted_loopback", "correct": true,
        }));
    }
    Ok(())
}

/// Script only the model's decisions; discovery, JavaScript execution, agent admission,
/// mailbox delivery, completion tracking, and shutdown all use the real runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn code_mode_only_discovers_spawns_and_receives_from_real_subagent() -> Result<()> {
    fn has_output(request: &wiremock::Request, call_id: &str) -> bool {
        serde_json::from_slice::<serde_json::Value>(&request.body).is_ok_and(|body| {
            body["input"].as_array().is_some_and(|items| {
                items.iter().any(|item| {
                    item["type"] == "custom_tool_call_output" && item["call_id"] == call_id
                })
            })
        })
    }

    fn output_text(
        request: &core_test_support::responses::ResponsesRequest,
        call_id: &str,
    ) -> String {
        let output = request.custom_tool_call_output(call_id);
        let (_, success) = request.custom_tool_call_output_content_and_success(call_id)
            .expect("probe output");
        assert_ne!(success, Some(false), "the probe cell must succeed");
        match &output["output"] {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(items) => items
                .iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            other => panic!("unexpected custom tool output: {other}"),
        }
    }

    let server = start_mock_server().await;
    let initial = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_last_message_input_text(request, "user", "Use a subagent for the runtime probe")
        },
        sse(vec![
            ev_response_created("probe-start"),
            ev_custom_tool_call("probe-spawn", "exec", r#"
const search = await tools.tool_search({query: "source:agents spawn_agent", limit: 1});
if (search.status === "aborted") throw Error("agent discovery aborted");
const spawn = resolve_tool("agents.spawn_agent");
const result = await spawn({task_name: "probe", message: "Report the runtime probe result to your parent"});
if (result.task_name !== "/root/probe") throw Error(JSON.stringify(result));
text("spawn-proof:" + JSON.stringify(result));
"#),
            ev_completed("probe-start"),
        ]),
    ).await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            has_task_capsule_objective(request, "Report the runtime probe result to your parent")
                && !has_output(request, "probe-message")
        },
        sse(vec![
            ev_response_created("worker-start"),
            ev_custom_tool_call(
                "probe-message",
                "exec",
                r#"
const send = resolve_tool("agents.send_message");
text(await send({target: "/root", message: "worker-mailbox-proof-7c49"}));
"#,
            ),
            ev_completed("worker-start"),
        ]),
    )
    .await;
    let worker_finish = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_output(request, "probe-message"),
        sse(vec![
            ev_response_created("worker-finish"),
            ev_assistant_message("worker-result", "worker-completion-proof-7c49"),
            ev_completed("worker-finish"),
        ]),
    )
    .await;
    let parent_wait = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            has_output(request, "probe-spawn") && !has_output(request, "probe-wait")
        },
        sse(vec![
            ev_response_created("parent-wait"),
            ev_custom_tool_call(
                "probe-wait",
                "exec",
                r#"
const wait = resolve_tool("agents.wait_agent");
const waited = await wait({timeout_ms: 1000});
if (waited.timed_out) throw Error(JSON.stringify(waited));
text("wait-proof:" + JSON.stringify(waited));
const listed = await resolve_tool("agents.list_agents")({});
if (!JSON.stringify(listed.agents).includes("/root/probe")) throw Error(JSON.stringify(listed));
text("list-proof:" + JSON.stringify(listed));
"#,
            ),
            ev_completed("parent-wait"),
        ]),
    )
    .await;
    let parent_finish = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| has_output(request, "probe-wait"),
        sse(vec![
            ev_response_created("parent-finish"),
            ev_assistant_message("parent-result", "probe complete"),
            ev_completed("parent-finish"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model_info_override("gpt-5.4", |model| model.supports_search_tool = true)
        .with_config(|config| {
            for feature in [
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::CodeMode,
                Feature::CodeModeOnly,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("enable agent code-mode probe");
            }
            config.multi_agent_v2.max_concurrent_threads_per_session = 2;
            config.multi_agent_v2.min_wait_timeout_ms = 0;
        });
    let test = builder.build(&server).await?;
    let outcome: Result<()> = async {
        let completed = tokio::time::timeout(
            Duration::from_secs(30),
            test.submit_turn_and_capture_completion("Use a subagent for the runtime probe"),
        )
        .await
        .context("parent agent lifecycle timed out")??;
        assert_eq!(completed.error, None);
        assert_eq!(completed.last_agent_message.as_deref(), Some("probe complete"));
        let first_request = initial.single_request().body_json();
        anyhow::ensure!(
            first_request["tools"]
                .as_array()
                .context("tool surface")?
                .iter()
                .all(|tool| { tool["name"] != "agents" && tool["name"] != "spawn_agent" }),
            "probe must exercise discovery rather than direct agent tools"
        );
        let developer_text = first_request["input"]
            .as_array()
            .context("initial input")?
            .iter()
            .filter(|item| item["role"] == "developer")
            .map(serde_json::Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::ensure!(
            developer_text.contains("may be called there"),
            "missing nested-call guidance"
        );
        anyhow::ensure!(
            !developer_text.contains("not from inside `functions.exec`"),
            "obsolete agent prohibition"
        );
        let spawned = output_text(&parent_wait.single_request(), "probe-spawn");
        anyhow::ensure!(spawned.contains("spawn-proof:"), "spawn failed: {spawned}");
        let finished = parent_finish.single_request();
        let waited = output_text(&finished, "probe-wait");
        anyhow::ensure!(
            waited.contains("wait-proof:") && waited.contains("list-proof:"),
            "lifecycle failed: {waited}"
        );
        let root_input = finished.body_json();
        anyhow::ensure!(
            root_input["input"]
                .as_array()
                .context("parent input")?
                .iter()
                .any(|item| {
                    item["role"] != "assistant"
                        && item.to_string().contains("worker-mailbox-proof-7c49")
                }),
            "child message never reached parent input"
        );
        anyhow::ensure!(
            worker_finish.requests().len() == 1,
            "child must execute its message tool"
        );
        let ids = test.thread_manager.list_thread_ids().await;
        anyhow::ensure!(
            ids.len() == 2,
            "expected one root and one real worker: {ids:?}"
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                for id in &ids {
                    let thread = test.thread_manager.get_thread(*id).await?;
                    if thread
                        .config_snapshot()
                        .await
                        .session_source
                        .get_agent_path()
                        .as_deref()
                        == Some("/root/probe")
                    {
                        let status = thread.agent_status().await;
                        match status {
                            AgentStatus::Completed(Some(ref message))
                                if message == "Agent-reported result (behavior unverified): worker-completion-proof-7c49" =>
                            {
                                return Ok::<(), anyhow::Error>(());
                            }
                            AgentStatus::PendingInit | AgentStatus::Running => {}
                            other => anyhow::bail!("unexpected worker terminal status: {other:?}"),
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("worker did not complete")??;
        Ok(())
    }
    .await;
    let shutdown = test
        .thread_manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    anyhow::ensure!(
        shutdown.timed_out.is_empty() && shutdown.submit_failed.is_empty(),
        "agent shutdown failed: {shutdown:?}"
    );
    outcome?;
    anyhow::ensure!(
        shutdown.completed.len() == 2,
        "both agent threads must shut down: {shutdown:?}"
    );
    Ok(())
}
