use super::*;
use serde_json::json;

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
    let stale = Arc::new(McpRuntimeSnapshot::new_with_manager_lifecycle(
        10,
        Arc::new(current.config().clone()),
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
