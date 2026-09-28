use super::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_mcp_tools_are_advertised_and_callable_while_an_optional_server_starts()
-> anyhow::Result<()> {
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolPayload;
    use crate::tools::parallel::ToolCallRuntime;
    use crate::tools::router::ToolCall;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::Request;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;

    let server = MockServer::start().await;
    let slow_started = Arc::new(tokio::sync::Notify::new());
    Mock::given(method("POST"))
        .respond_with({
            let slow_started = Arc::clone(&slow_started);
            move |request: &Request| {
                let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
                let initializing = body["method"] == "initialize";
                let result = match body["method"].as_str().unwrap() {
                    "initialize" => json!({
                        "protocolVersion": body["params"]["protocolVersion"],
                        "capabilities": {"tools": {}, "resources": {}},
                        "serverInfo": {"name": "sampling-test", "version": "1"},
                    }),
                    "notifications/initialized" => return ResponseTemplate::new(202),
                    "tools/list" => json!({"tools": [{
                        "name": "echo", "inputSchema": {"type": "object", "properties": {}},
                        "annotations": {"readOnlyHint": true},
                    }]}),
                    "tools/call" => json!({
                        "content": [{"type": "text", "text": "ready tool executed"}],
                        "isError": false,
                    }),
                    _ => return ResponseTemplate::new(400),
                };
                let response = ResponseTemplate::new(200)
                    .set_body_json(json!({"jsonrpc": "2.0", "id": body["id"], "result": result}));
                if initializing && request.url.path() == "/slow" {
                    slow_started.notify_one();
                    // The test cancels this pending initialization; it never waits for the delay.
                    response.set_delay(std::time::Duration::from_secs(30))
                } else {
                    response
                }
            }
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    let (session, mut turn, _events) =
        crate::session::tests::make_session_and_context_with_rx().await;
    let turn_mut = Arc::get_mut(&mut turn).unwrap();
    turn_mut.model_info.supports_search_tool = false;
    turn_mut.model_info.tool_mode = Some(codex_protocol::openai_models::ToolMode::Direct);
    let config = Arc::make_mut(&mut turn_mut.config);
    config.features.disable(Feature::ToolSuggest)?;
    config.mcp_servers.set(serde_json::from_value(json!({
        "ready": {"url": format!("{}/ready", server.uri())},
        "slow": {"url": format!("{}/slow", server.uri()), "required": false, "startup_timeout_sec": 60},
    }))?)?;
    session.refresh_mcp_servers_now(&turn, &turn.config).await;
    let manager = session.services.latest_mcp_runtime();
    assert!(
        manager
            .manager()
            .wait_for_server_ready("ready", std::time::Duration::from_secs(5))
            .await
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), slow_started.notified()).await?;
    let step = session.capture_step_context(Arc::clone(&turn)).await?;
    let snapshot =
        tokio::time::timeout(std::time::Duration::from_secs(1), step.mcp_tool_snapshot()).await?;
    assert!(!snapshot.temporarily_unavailable);
    assert_eq!(snapshot.pending_servers, ["slow"]);
    assert_eq!(snapshot.tools.len(), 1);
    assert_eq!(snapshot.tools[0].server_name, "ready");
    assert!(snapshot.resources_available);
    let notice = snapshot.availability_notice().unwrap();
    assert!(
        notice.contains("slow") && notice.contains("Other advertised capabilities remain usable")
    );
    let tool_name = snapshot.tools[0].canonical_tool_name();
    let router =
        super::super::turn::built_tools(&session, &step, &[], &CancellationToken::new()).await?;
    assert!(
        router
            .model_visible_specs()
            .iter()
            .any(|spec| spec.name().contains("ready"))
    );
    assert!(
        !router
            .model_visible_specs()
            .iter()
            .any(|spec| spec.name().contains("slow"))
    );
    assert_eq!(
        router.exposure_identity().mcp_tool_catalog_revision,
        snapshot.revision
    );
    step.set_tool_router(router)
        .map_err(|_| anyhow::anyhow!("unexpected finalized router"))?;
    let runtime = ToolCallRuntime::new(
        Arc::clone(&session),
        Arc::clone(&step),
        Arc::new(tokio::sync::Mutex::new(
            crate::turn_diff_tracker::TurnDiffTracker::new(),
        )),
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        runtime.handle_tool_call_with_source(
            ToolCall {
                tool_name,
                call_id: "ready-call".into(),
                payload: ToolPayload::Function {
                    arguments: "{}".into(),
                },
            },
            ToolCallSource::Direct,
            CancellationToken::new(),
        ),
    )
    .await
    .expect("a ready tool call must not wait for the optional server")?;
    assert_eq!(
        result.code_mode_result()["content"][0]["text"],
        "ready tool executed"
    );
    session.cancel_mcp_startup().await;
    assert!(
        !manager
            .manager()
            .wait_for_server_ready("slow", std::time::Duration::from_secs(1))
            .await
    );
    let after_cancel = session.capture_step_context(Arc::clone(&turn)).await?;
    let after_cancel = after_cancel.mcp_tool_snapshot().await;
    assert!(after_cancel.pending_servers.is_empty());
    assert!(after_cancel.revision > snapshot.revision);
    assert_eq!(after_cancel.tools.len(), 1);
    assert_eq!(
        snapshot.pending_servers,
        ["slow"],
        "the advertised step stays frozen"
    );
    manager.manager().shutdown().await;
    Ok(())
}

#[tokio::test]
async fn canceled_mcp_refresh_returns_an_error_for_a_mismatched_environment() {
    let (session, turn, _) = crate::session::tests::make_session_and_context_with_rx().await;
    let current = session.services.latest_mcp_runtime();
    let mut stale_config = current.config().clone();
    stale_config.mcp_server_catalog =
        stale_config
            .mcp_server_catalog
            .with_materialized_servers(std::collections::HashMap::from([(
                "removed-server".to_string(),
                serde_json::from_value(json!({
                    "command": "unused-mcp-test-server"
                }))
                .unwrap(),
            )]));
    let stale = Arc::new(McpRuntimeSnapshot::new_with_manager_lifecycle(
        10,
        Arc::new(stale_config),
        current.plugins_available(),
        current.manager_arc(),
        current.manager_lifecycle_arc(),
        current.runtime_context().clone(),
        vec!["removed-environment".to_string()],
    ));
    session.services.mcp_runtime.store(Some(stale));
    session
        .services
        .mcp_startup_cancellation_token
        .lock()
        .await
        .cancel();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        session.mcp_runtime_for_step(&turn, &turn.environments, &[]),
    )
    .await
    .expect("canceled refresh must not spin");
    assert!(matches!(
        result,
        Err(codex_protocol::error::CodexErr::TurnAborted)
    ));
    let matching = Arc::new(McpRuntimeSnapshot::new_with_manager_lifecycle(
        11,
        Arc::new(current.config().clone()),
        current.plugins_available(),
        current.manager_arc(),
        current.manager_lifecycle_arc(),
        current.runtime_context().clone(),
        Vec::new(),
    ));
    session
        .services
        .mcp_runtime
        .store(Some(Arc::clone(&matching)));
    let result = session
        .mcp_runtime_for_step(&turn, &turn.environments, &[])
        .await
        .unwrap();
    assert!(
        Arc::ptr_eq(&result, &matching),
        "a coherent published runtime remains usable"
    );
}

#[tokio::test]
async fn canceled_mcp_refresh_allows_environment_only_updates_without_restarting_a_manager() {
    let (session, turn, _) = crate::session::tests::make_session_and_context_with_rx().await;
    let current = session.services.latest_mcp_runtime();
    let projection = session
        .services
        .mcp_manager
        .runtime_config_for_step(
            &turn.config,
            &session.services.mcp_thread_init,
            &session.services.thread_extension_data,
            &turn.originator,
            &[],
        )
        .await;
    let stale = Arc::new(McpRuntimeSnapshot::new_with_manager_lifecycle(
        10,
        Arc::new(projection.config),
        current.plugins_available(),
        current.manager_arc(),
        current.manager_lifecycle_arc(),
        current.runtime_context().clone(),
        vec!["removed-local-environment".to_string()],
    ));
    session.services.mcp_runtime.store(Some(stale));
    session
        .services
        .mcp_startup_cancellation_token
        .lock()
        .await
        .cancel();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        session.mcp_runtime_for_step(&turn, &turn.environments, &[]),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.available_environment_ids().is_empty());
    assert!(Arc::ptr_eq(&result.manager_arc(), &current.manager_arc()));
}

#[test]
fn plugin_install_elicitation_telemetry_metadata_requires_install_tool_suggestion() {
    let event = EventMsg::ElicitationRequest(ElicitationRequestEvent {
        turn_id: Some("turn-1".to_string()),
        server_name: "codex_apps".to_string(),
        id: codex_protocol::mcp::RequestId::String("request-1".to_string()),
        request: codex_protocol::approvals::ElicitationRequest::Form {
            meta: Some(json!({
                "codex_approval_kind": "tool_suggestion",
                "suggest_type": "install",
                "tool_type": "plugin",
                "tool_id": "slack@openai-curated",
                "tool_name": "Slack",
            })),
            message: "Install Slack?".to_string(),
            requested_schema: json!({
                "type": "object",
                "properties": {},
            }),
        },
    });

    assert_eq!(
        plugin_install_elicitation_telemetry_metadata(&event),
        Some(PluginInstallElicitationTelemetryMetadata {
            tool_type: "plugin".to_string(),
            tool_id: "slack@openai-curated".to_string(),
            tool_name: "Slack".to_string(),
        })
    );

    let enable_event = EventMsg::ElicitationRequest(ElicitationRequestEvent {
        turn_id: Some("turn-1".to_string()),
        server_name: "codex_apps".to_string(),
        id: codex_protocol::mcp::RequestId::String("request-2".to_string()),
        request: codex_protocol::approvals::ElicitationRequest::Form {
            meta: Some(json!({
                "codex_approval_kind": "tool_suggestion",
                "suggest_type": "enable",
                "tool_type": "plugin",
                "tool_id": "slack@openai-curated",
                "tool_name": "Slack",
            })),
            message: "Enable Slack?".to_string(),
            requested_schema: json!({
                "type": "object",
                "properties": {},
            }),
        },
    });

    assert_eq!(
        plugin_install_elicitation_telemetry_metadata(&enable_event),
        None
    );
}
