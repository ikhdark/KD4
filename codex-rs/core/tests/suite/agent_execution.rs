use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_protocol::protocol::AgentStatus;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::request_has_last_message_input_text;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
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
        |request: &wiremock::Request| has_function_call_output(request, "first-call"),
        sse(vec![
            ev_response_created("first-followup-response"),
            ev_assistant_message("first-followup-message", "spawned"),
            ev_completed("first-followup-response"),
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
    });
    let test = builder.build(&server).await?;
    test.submit_turn(FIRST_PROMPT).await?;

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
        tokio::time::timeout(
            Duration::from_secs(30),
            test.submit_turn("Use a subagent for the runtime probe"),
        )
        .await
        .context("parent agent lifecycle timed out")??;
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
