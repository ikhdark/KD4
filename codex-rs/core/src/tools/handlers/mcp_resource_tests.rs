use super::*;
use std::collections::HashMap;

use crate::session::McpRuntimeSnapshot;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context_with_rx;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_mcp::CodexAppsToolsCache;
use codex_mcp::EffectiveMcpServer;
use codex_mcp::ElicitationRequestRouter;
use codex_mcp::McpConnectionManager;
use codex_mcp::McpServerCollection;
use codex_mcp::McpServerCollectionError;
use codex_mcp::ToolPluginProvenance;
use codex_protocol::protocol::EventMsg;
use codex_tools::ToolName;
use pretty_assertions::assert_eq;
use rmcp::model::AnnotateAble;
use rmcp::model::ElicitationCapability;
use rmcp::model::ResourceContents;
use serde_json::json;
use tokio::sync::Mutex;

fn resource(uri: &str, name: &str) -> Resource {
    rmcp::model::RawResource {
        uri: uri.to_string(),
        name: name.to_string(),
        title: None,
        description: None,
        mime_type: None,
        size: None,
        icons: None,
        meta: None,
    }
    .no_annotation()
}

fn template(uri_template: &str, name: &str) -> ResourceTemplate {
    rmcp::model::RawResourceTemplate {
        uri_template: uri_template.to_string(),
        name: name.to_string(),
        title: None,
        description: None,
        mime_type: None,
        icons: None,
    }
    .no_annotation()
}

async fn step_context_with_blocked_mcp_server(
    turn: &Arc<TurnContext>,
    server_name: &str,
) -> (
    Arc<StepContext>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind blocking MCP test server");
    let server_address = listener
        .local_addr()
        .expect("blocking MCP test server address");
    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
    let blocking_server = tokio::spawn(async move {
        let (connection, _) = listener
            .accept()
            .await
            .expect("accept blocking MCP connection");
        let _ = connected_tx.send(());
        let _connection = connection;
        std::future::pending::<()>().await;
    });

    let mcp_servers = HashMap::from([(
        server_name.to_string(),
        EffectiveMcpServer::configured(McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: format!("http://{server_address}/mcp"),
                bearer_token_env_var: None,
                http_headers: None,
                env_http_headers: None,
            },
            environment_id: "local".to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(60)),
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        }),
    )]);
    let base_mcp = McpRuntimeSnapshot::new_uninitialized_for_test(turn.config.as_ref());
    let runtime_context = base_mcp.runtime_context().clone();
    let (tx_event, _rx_event) = async_channel::unbounded();
    let startup_cancellation_token = CancellationToken::new();
    let manager = McpConnectionManager::new(
        &mcp_servers,
        turn.config.mcp_oauth_credentials_store_mode,
        turn.config.auth_keyring_backend_kind(),
        HashMap::new(),
        &turn.approval_policy,
        turn.sub_id.clone(),
        tx_event,
        startup_cancellation_token.clone(),
        turn.permission_profile(),
        runtime_context.clone(),
        turn.config.codex_home.to_path_buf(),
        CodexAppsToolsCache::default(),
        codex_mcp::codex_apps_tools_cache_key(None),
        turn.config.prefix_mcp_tool_names(),
        ElicitationCapability::default(),
        /*supports_openai_form_elicitation*/ false,
        ToolPluginProvenance::default(),
        /*auth*/ None,
        /*codex_apps_auth_manager*/ None,
        /*elicitation_reviewer*/ None,
        /*elicitation_lifecycle*/ None,
        ElicitationRequestRouter::default(),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(2), connected_rx)
        .await
        .expect("blocking MCP server connection timed out")
        .expect("blocking MCP server connection sender dropped");

    let mcp = Arc::new(McpRuntimeSnapshot::new(
        Arc::new(base_mcp.config().clone()),
        base_mcp.plugins_available(),
        Arc::new(manager),
        runtime_context,
        base_mcp.available_environment_ids().to_vec(),
    ));
    let step_context = Arc::new(StepContext::new(
        Arc::clone(turn),
        turn.environments.clone(),
        Vec::new(),
        mcp,
        /*loaded_agents_md*/ None,
    ));

    (step_context, startup_cancellation_token, blocking_server)
}

#[test]
fn resource_with_server_serializes_server_field() {
    let entry = ResourceWithServer::new("test".to_string(), resource("memo://id", "memo"));
    let value = serde_json::to_value(&entry).expect("serialize resource");

    assert_eq!(value["server"], json!("test"));
    assert_eq!(value["uri"], json!("memo://id"));
    assert_eq!(value["name"], json!("memo"));
}

#[test]
fn list_resources_payload_from_single_server_copies_next_cursor() {
    let result = ListResourcesResult {
        meta: None,
        next_cursor: Some("cursor-1".to_string()),
        resources: vec![resource("memo://id", "memo")],
    };
    let payload = ListResourcesPayload::from_single_server(
        "srv".to_string(),
        result,
        TruncationPolicy::Bytes(1_024),
    )
    .expect("build payload");
    let value = serde_json::to_value(&payload).expect("serialize payload");

    assert_eq!(value["server"], json!("srv"));
    assert_eq!(value["nextCursor"], json!("cursor-1"));
    let resources = value["resources"].as_array().expect("resources array");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["server"], json!("srv"));
}

#[test]
fn list_resources_payload_from_all_servers_is_sorted() {
    let mut map = HashMap::new();
    map.insert(
        "beta".to_string(),
        ListResourcesResult {
            meta: None,
            next_cursor: None,
            resources: vec![resource("memo://b-1", "b-1")],
        },
    );
    map.insert(
        "alpha".to_string(),
        ListResourcesResult {
            meta: None,
            next_cursor: None,
            resources: vec![resource("memo://a-1", "a-1"), resource("memo://a-2", "a-2")],
        },
    );

    let payload = ListResourcesPayload::from_all_servers(
        McpServerCollection {
            results: map,
            errors: Vec::new(),
        },
        TruncationPolicy::Bytes(1_024),
    )
    .expect("build payload");
    let value = serde_json::to_value(&payload).expect("serialize payload");
    let uris: Vec<String> = value["resources"]
        .as_array()
        .expect("resources array")
        .iter()
        .map(|entry| entry["uri"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(
        uris,
        vec![
            "memo://a-1".to_string(),
            "memo://a-2".to_string(),
            "memo://b-1".to_string()
        ]
    );
}

#[test]
fn call_tool_result_from_content_marks_success() {
    let result = call_tool_result_from_content("{}", Some(true));
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
}

#[test]
fn parse_arguments_handles_empty_and_json() {
    assert!(
        parse_arguments(" \n\t").unwrap().is_none(),
        "expected None for empty arguments"
    );

    assert!(
        parse_arguments("null").unwrap().is_none(),
        "expected None for null arguments"
    );

    let value = parse_arguments(r#"{"server":"figma"}"#)
        .expect("parse json")
        .expect("value present");
    assert_eq!(value["server"], json!("figma"));
}

#[test]
fn list_args_reject_unknown_selectors() {
    let resources_err = parse_args_with_default::<ListResourcesArgs>(Some(json!({
        "sever": "calendar"
    })))
    .expect_err("misspelled resource selector should fail");
    let templates_err = parse_args_with_default::<ListResourceTemplatesArgs>(Some(json!({
        "sever": "calendar"
    })))
    .expect_err("misspelled template selector should fail");

    assert!(resources_err.to_string().contains("unknown field `sever`"));
    assert!(templates_err.to_string().contains("unknown field `sever`"));
}

#[test]
fn optional_selectors_distinguish_omitted_from_blank_values() {
    let resources_without_arguments = parse_args_with_default::<ListResourcesArgs>(None)
        .expect("omitted resource arguments should parse")
        .normalize()
        .expect("omitted resource selectors should retain defaults");
    let resources_empty_object = parse_args_with_default::<ListResourcesArgs>(Some(json!({})))
        .expect("empty resource arguments should parse")
        .normalize()
        .expect("empty resource selectors should retain defaults");
    let templates_without_arguments = parse_args_with_default::<ListResourceTemplatesArgs>(None)
        .expect("omitted template arguments should parse")
        .normalize()
        .expect("omitted template selectors should retain defaults");
    let templates_empty_object =
        parse_args_with_default::<ListResourceTemplatesArgs>(Some(json!({})))
            .expect("empty template arguments should parse")
            .normalize()
            .expect("empty template selectors should retain defaults");
    assert_eq!(resources_without_arguments.server, None);
    assert_eq!(resources_without_arguments.cursor, None);
    assert_eq!(resources_empty_object.server, None);
    assert_eq!(resources_empty_object.cursor, None);
    assert_eq!(templates_without_arguments.server, None);
    assert_eq!(templates_without_arguments.cursor, None);
    assert_eq!(templates_empty_object.server, None);
    assert_eq!(templates_empty_object.cursor, None);

    let opaque_cursor = "  opaque cursor token\t";
    let resources_with_values = parse_args_with_default::<ListResourcesArgs>(Some(json!({
        "server": "  calendar  ",
        "cursor": opaque_cursor,
    })))
    .expect("padded resource selectors should parse")
    .normalize()
    .expect("nonblank resource selectors should normalize");
    let templates_with_values = parse_args_with_default::<ListResourceTemplatesArgs>(Some(json!({
        "server": "  templates  ",
        "cursor": opaque_cursor,
    })))
    .expect("padded template selectors should parse")
    .normalize()
    .expect("nonblank template selectors should normalize");
    assert_eq!(resources_with_values.server.as_deref(), Some("calendar"));
    assert_eq!(resources_with_values.cursor.as_deref(), Some(opaque_cursor));
    assert_eq!(templates_with_values.server.as_deref(), Some("templates"));
    assert_eq!(templates_with_values.cursor.as_deref(), Some(opaque_cursor));

    for error in [
        ListResourcesArgs {
            server: Some("   ".to_string()),
            cursor: None,
        }
        .normalize()
        .expect_err("blank resource server should fail"),
        ListResourceTemplatesArgs {
            server: Some("   ".to_string()),
            cursor: None,
        }
        .normalize()
        .expect_err("blank template server should fail"),
    ] {
        assert!(error.to_string().contains("server must not be blank"));
    }

    for error in [
        ListResourcesArgs {
            server: Some("calendar".to_string()),
            cursor: Some("\n\t".to_string()),
        }
        .normalize()
        .expect_err("blank resource cursor should fail"),
        ListResourceTemplatesArgs {
            server: Some("calendar".to_string()),
            cursor: Some("\n\t".to_string()),
        }
        .normalize()
        .expect_err("blank template cursor should fail"),
    ] {
        assert!(error.to_string().contains("cursor must not be blank"));
    }
}

#[test]
fn template_with_server_serializes_server_field() {
    let entry = ResourceTemplateWithServer::new("srv".to_string(), template("memo://{id}", "memo"));
    let value = serde_json::to_value(&entry).expect("serialize template");

    assert_eq!(
        value,
        json!({
            "server": "srv",
            "uriTemplate": "memo://{id}",
            "name": "memo"
        })
    );
}

#[test]
fn serialize_function_output_preserves_small_payload() {
    let payload = json!({"server": "hosted", "resources": []});
    let expected = serde_json::to_string(&payload).expect("serialize payload");

    let output = serialize_function_output(payload, TruncationPolicy::Bytes(1_024))
        .expect("serialize function output")
        .into_text();

    assert_eq!(output, expected);
}

#[test]
fn aggregate_resources_keep_whole_pages_and_continuation_metadata() {
    let truncation_policy = TruncationPolicy::Bytes(650);
    let mut pages = HashMap::new();
    pages.insert(
        "alpha".to_string(),
        ListResourcesResult {
            meta: None,
            next_cursor: Some("alpha-next".to_string()),
            resources: vec![
                resource("memo://alpha/id", &format!("alpha-{}", "x".repeat(350))),
                resource("memo://alpha/name", &format!("alpha-{}", "y".repeat(350))),
            ],
        },
    );
    pages.insert(
        "beta".to_string(),
        ListResourcesResult {
            meta: None,
            next_cursor: Some("beta-next".to_string()),
            resources: vec![resource("memo://beta/id", "beta")],
        },
    );

    let payload = ListResourcesPayload::from_all_servers(
        McpServerCollection {
            results: pages,
            errors: Vec::new(),
        },
        truncation_policy,
    )
    .expect("build bounded resource payload");
    let value = serde_json::to_value(payload).expect("serialize aggregate resource payload");

    assert_eq!(value["remainingServers"], json!(["alpha"]));
    assert_eq!(value["nextCursors"], json!({"beta": "beta-next"}));
    assert_eq!(value["resources"].as_array().unwrap().len(), 1);
    assert_eq!(value["resources"][0]["server"], json!("beta"));
    assert_eq!(value["omittedCount"], json!(2));
    assert_eq!(value["truncated"], json!(true));
}

#[test]
fn single_server_pages_fail_instead_of_skipping_entries_before_next_cursor() {
    let truncation_policy = TruncationPolicy::Bytes(650);
    let resource_error = ListResourcesPayload::from_single_server(
        "srv".to_string(),
        ListResourcesResult {
            meta: None,
            next_cursor: Some("opaque-resource-cursor".to_string()),
            resources: (0..4)
                .map(|index| {
                    resource(
                        &format!("memo://{index}"),
                        &format!("resource-{index}-{}", "x".repeat(300)),
                    )
                })
                .collect(),
        },
        truncation_policy,
    )
    .expect_err("oversized resource page must fail intact");
    let template_error = ListResourceTemplatesPayload::from_single_server(
        "srv".to_string(),
        ListResourceTemplatesResult {
            meta: None,
            next_cursor: Some("opaque-template-cursor".to_string()),
            resource_templates: (0..4)
                .map(|index| {
                    template(
                        &format!("memo://template/{index}/{{id}}"),
                        &format!("template-{index}-{}", "y".repeat(300)),
                    )
                })
                .collect(),
        },
        truncation_policy,
    )
    .expect_err("oversized template page must fail intact");

    assert!(
        resource_error
            .to_string()
            .contains("before its next cursor")
    );
    assert!(
        template_error
            .to_string()
            .contains("before its next cursor")
    );
}

#[test]
fn aggregate_templates_keep_whole_pages_and_continuation_metadata() {
    let truncation_policy = TruncationPolicy::Bytes(700);
    let mut pages = HashMap::new();
    pages.insert(
        "alpha".to_string(),
        ListResourceTemplatesResult {
            meta: None,
            next_cursor: Some("alpha-next".to_string()),
            resource_templates: vec![
                template("memo://alpha/{id}", &format!("alpha-{}", "x".repeat(350))),
                template("memo://alpha/{name}", &format!("alpha-{}", "y".repeat(350))),
            ],
        },
    );
    pages.insert(
        "beta".to_string(),
        ListResourceTemplatesResult {
            meta: None,
            next_cursor: Some("beta-next".to_string()),
            resource_templates: vec![template("memo://beta/{id}", "beta")],
        },
    );

    let payload = ListResourceTemplatesPayload::from_all_servers(
        McpServerCollection {
            results: pages,
            errors: Vec::new(),
        },
        truncation_policy,
    )
    .expect("build aggregate template payload");
    let value = serde_json::to_value(payload).expect("serialize aggregate template payload");

    assert_eq!(value["remainingServers"], json!(["alpha"]));
    assert_eq!(value["nextCursors"], json!({"beta": "beta-next"}));
    assert_eq!(value["resourceTemplates"].as_array().unwrap().len(), 1);
    assert_eq!(value["resourceTemplates"][0]["server"], json!("beta"));
    assert_eq!(value["omittedCount"], json!(2));
    assert_eq!(value["truncated"], json!(true));
}

#[test]
fn aggregate_resource_errors_are_disclosed_and_all_failure_is_not_success() {
    let mixed = ListResourcesPayload::from_all_servers(
        McpServerCollection {
            results: HashMap::from([(
                "ready".to_string(),
                ListResourcesResult {
                    meta: None,
                    next_cursor: None,
                    resources: Vec::new(),
                },
            )]),
            errors: vec![McpServerCollectionError {
                server: "needs-auth".to_string(),
                message: "server requires authentication".to_string(),
            }],
        },
        TruncationPolicy::Bytes(1_024),
    )
    .expect("partial success should remain usable");
    let mixed = serde_json::to_value(mixed).expect("serialize mixed result");
    assert_eq!(
        mixed["errors"],
        json!([{"server": "needs-auth", "message": "server requires authentication"}])
    );

    let error = ListResourcesPayload::from_all_servers(
        McpServerCollection {
            results: HashMap::new(),
            errors: vec![McpServerCollectionError {
                server: "offline".to_string(),
                message: "server unavailable".to_string(),
            }],
        },
        TruncationPolicy::Bytes(1_024),
    )
    .expect_err("all-server failure must not look like an empty success");
    assert!(error.to_string().contains("every selected server"));
    assert!(error.to_string().contains("offline"));
}

#[test]
fn serialize_function_output_bounds_large_read_inside_text_field() {
    let truncation_policy = TruncationPolicy::Bytes(8_000);
    let original_text = "x".repeat(16_000);
    let payload = ReadResourcePayload::new(
        "hosted".to_string(),
        "skill://large/SKILL.md".to_string(),
        ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
            uri: "skill://large/SKILL.md".to_string(),
            mime_type: Some("text/markdown".to_string()),
            text: original_text.clone(),
            meta: None,
        }]),
        truncation_policy,
    )
    .expect("build bounded read payload");
    let output = serialize_function_output(payload, truncation_policy)
        .expect("serialize bounded function output")
        .into_text();
    let value: Value = serde_json::from_str(&output).expect("bounded read must remain valid JSON");
    let bounded_text = value["contents"][0]["text"]
        .as_str()
        .expect("bounded text content");

    assert_eq!(value["server"], json!("hosted"));
    assert_eq!(value["uri"], json!("skill://large/SKILL.md"));
    assert_eq!(value["contents"][0]["uri"], json!("skill://large/SKILL.md"));
    assert_eq!(value["truncated"], json!(true));
    assert!(bounded_text.len() < original_text.len());
    assert!(bounded_text.contains("truncated"));
}

#[test]
fn oversized_blob_is_replaced_by_explicit_bounded_metadata() {
    let truncation_policy = TruncationPolicy::Bytes(512);
    let payload = ReadResourcePayload::new(
        "hosted".to_string(),
        "blob://large".to_string(),
        ReadResourceResult::new(vec![ResourceContents::BlobResourceContents {
            uri: "blob://large".to_string(),
            mime_type: Some("application/octet-stream".to_string()),
            blob: "a".repeat(8_000),
            meta: None,
        }]),
        truncation_policy,
    )
    .expect("build bounded blob payload");
    let value = serde_json::to_value(payload).expect("serialize bounded blob payload");

    assert_eq!(value["contents"][0]["uri"], json!("blob://large"));
    assert_eq!(value["contents"][0]["omitted"], json!(true));
    assert!(value["contents"][0].get("blob").is_none());
    assert_eq!(value["truncated"], json!(true));
}

#[test]
fn resource_handlers_wait_for_runtime_cancellation_cleanup() {
    assert!(ListMcpResourcesHandler.waits_for_runtime_cancellation());
    assert!(ListMcpResourceTemplatesHandler.waits_for_runtime_cancellation());
    assert!(ReadMcpResourceHandler.waits_for_runtime_cancellation());
}

#[tokio::test]
async fn cancelled_read_resource_handler_emits_one_failed_terminal_item() {
    const CALL_ID: &str = "call-cancelled-resource";
    const SERVER_NAME: &str = "blocked";

    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let (step_context, startup_cancellation_token, blocking_server) =
        step_context_with_blocked_mcp_server(&turn, SERVER_NAME).await;
    let cancellation_token = CancellationToken::new();
    let handler_task = tokio::spawn({
        let session = Arc::clone(&session);
        let turn = Arc::clone(&turn);
        let cancellation_token = cancellation_token.clone();
        async move {
            ReadMcpResourceHandler
                .handle(ToolInvocation {
                    session,
                    step_context,
                    turn,
                    cancellation_token,
                    tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                    call_id: CALL_ID.to_string(),
                    tool_name: ToolName::plain("read_mcp_resource"),
                    source: ToolCallSource::Direct,
                    payload: ToolPayload::Function {
                        arguments: json!({
                            "server": SERVER_NAME,
                            "uri": "memo://blocked",
                        })
                        .to_string(),
                    },
                })
                .await
        }
    });

    let started = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx_event.recv().await.expect("resource lifecycle event");
            if let EventMsg::ItemStarted(started) = event.msg
                && let TurnItem::McpToolCall(started) = started.item
                && started.id == CALL_ID
            {
                break started;
            }
        }
    })
    .await
    .expect("blocked resource handler should emit an InProgress item");
    assert_eq!(started.status, McpToolCallStatus::InProgress);

    cancellation_token.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), handler_task)
        .await
        .expect("cancelled resource handler timed out")
        .expect("cancelled resource handler task panicked");
    let error = match result {
        Ok(_) => panic!("cancellation should fail the in-flight resource operation"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains(MCP_RESOURCE_CALL_CANCELLED_MESSAGE)
    );

    let completed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx_event.recv().await.expect("resource lifecycle event");
            if let EventMsg::ItemCompleted(completed) = event.msg
                && let TurnItem::McpToolCall(completed) = completed.item
                && completed.id == CALL_ID
            {
                break completed;
            }
        }
    })
    .await
    .expect("cancelled resource handler should emit a terminal item");
    assert_eq!(completed.status, McpToolCallStatus::Failed);
    assert_eq!(
        completed.error.expect("cancellation error").message,
        MCP_RESOURCE_CALL_CANCELLED_MESSAGE
    );
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::ItemCompleted(completed) = event.msg
            && let TurnItem::McpToolCall(completed) = completed.item
            && completed.id == CALL_ID
        {
            panic!("exactly one terminal item expected");
        }
    }

    startup_cancellation_token.cancel();
    blocking_server.abort();
    let _ = blocking_server.await;
}

#[tokio::test]
async fn failed_resource_call_emits_one_failed_terminal_item() {
    const CALL_ID: &str = "call-failed-resource";
    const FAILURE_MESSAGE: &str = "resource operation failed";

    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let result = execute_resource_call(
        &session,
        &turn,
        CALL_ID,
        McpInvocation {
            server: "offline".to_string(),
            tool: "list_mcp_resources".to_string(),
            arguments: None,
        },
        CancellationToken::new(),
        async {
            Err::<FunctionToolOutput, FunctionCallError>(FunctionCallError::RespondToModel(
                FAILURE_MESSAGE.to_string(),
            ))
        },
    )
    .await;
    let error = match result {
        Ok(_) => panic!("resource operation failure should be returned"),
        Err(error) => error,
    };
    assert!(error.to_string().contains(FAILURE_MESSAGE));

    let completed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx_event.recv().await.expect("resource lifecycle event");
            if let EventMsg::ItemCompleted(completed) = event.msg
                && let TurnItem::McpToolCall(completed) = completed.item
                && completed.id == CALL_ID
            {
                break completed;
            }
        }
    })
    .await
    .expect("failed resource call should emit a terminal item");
    assert_eq!(completed.status, McpToolCallStatus::Failed);
    assert_eq!(
        completed.error.expect("resource failure").message,
        FAILURE_MESSAGE
    );
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::ItemCompleted(completed) = event.msg
            && let TurnItem::McpToolCall(completed) = completed.item
            && completed.id == CALL_ID
        {
            panic!("exactly one terminal item expected");
        }
    }
}

#[tokio::test]
async fn successful_resource_call_emits_one_completed_terminal_item() {
    const CALL_ID: &str = "call-successful-resource";

    let (session, turn, rx_event) = make_session_and_context_with_rx().await;
    let output = execute_resource_call(
        &session,
        &turn,
        CALL_ID,
        McpInvocation {
            server: "ready".to_string(),
            tool: "list_mcp_resources".to_string(),
            arguments: None,
        },
        CancellationToken::new(),
        async { Ok(FunctionToolOutput::from_text("{}".to_string(), Some(true))) },
    )
    .await
    .expect("resource operation should succeed");
    assert!(output.success_for_logging());

    let completed = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let event = rx_event.recv().await.expect("resource lifecycle event");
            if let EventMsg::ItemCompleted(completed) = event.msg
                && let TurnItem::McpToolCall(completed) = completed.item
                && completed.id == CALL_ID
            {
                break completed;
            }
        }
    })
    .await
    .expect("successful resource call should emit a terminal item");
    assert_eq!(completed.status, McpToolCallStatus::Completed);
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::ItemCompleted(completed) = event.msg
            && let TurnItem::McpToolCall(completed) = completed.item
            && completed.id == CALL_ID
        {
            panic!("exactly one terminal item expected");
        }
    }
}
