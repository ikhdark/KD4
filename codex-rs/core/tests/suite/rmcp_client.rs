use anyhow::Context as _;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_config::types::McpServerAuth;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerEnvVar;
use codex_config::types::McpServerTransportConfig;
use codex_core::config::Config;
use codex_exec_server::CreateDirectoryOptions;
use codex_http_client::HttpClientBuilder;
use codex_login::CodexAuth;
use codex_mcp::MCP_SANDBOX_STATE_META_CAPABILITY;
use codex_mcp::SandboxState;
use codex_models_manager::manager::RefreshStrategy;
use codex_utils_path_uri::LegacyAppPathString;

use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::McpInvocation;
use codex_protocol::protocol::McpToolCallBeginEvent;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use codex_utils_cargo_bin::cargo_bin;
use codex_utils_path_uri::PathUri;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::assert_regex_match;
use core_test_support::responses;
use core_test_support::responses::mount_models_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use core_test_support::wait_for_mcp_server;
use http::StatusCode;
use image::DynamicImage;
use image::GenericImageView;
use image::ImageBuffer;
use image::Rgba;
use serde_json::Value;
use serde_json::json;
use serial_test::serial;
use std::io::Cursor;
use tempfile::tempdir;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::Instant;
use tokio::time::sleep;
use wiremock::MockServer;

static OPENAI_PNG: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAD0AAAA9CAYAAAAeYmHpAAAE6klEQVR4Aeyau44UVxCGx1fZsmRLlm3Zoe0XcGQ5cUiCCIgJeS9CHgAhMkISQnIuGQgJEkBcxLW+nqnZ6uqqc+nuWRC7q/P3qetf9e+MtOwyX25O4Nep6JPyop++0qev9HrfgZ+F6r2DuB/vHOrt/UIkqdDHYvujOW6fO7h/CNEI+a5jc+pBR8uy0jVFsziYu5HtfSUk+Io34q921hLNctFSX0gwww+S8wce8K1LfCU+cYW4888aov8NxqvQILUPPReLOrm6zyLxa4i+6VZuFbJo8d1MOHZm+7VUtB/aIvhPWc/3SWg49JcwFLlHxuXKjtyloo+YNhuW3VS+WPBuUEMvCFKjEDVgFBQHXrnazpqiSxNZCkQ1kYiozsbm9Oz7l4i2Il7vGccGNWAc3XosDrZe/9P3ZnMmzHNEQw4smf8RQ87XEAMsC7Az0Au+dgXerfH4+sHvEc0SYGic8WBBUGqFH2gN7yDrazy7m2pbRTeRmU3+MjZmr1h6LJgPbGy23SI6GlYT0brQ71IY8Us4PNQCm+zepSbaD2BY9xCaAsD9IIj/IzFmKMSdHHonwdZATbTnYREf6/VZGER98N9yCWIvXQwXDoDdhZJoT8jwLnJXDB9w4Sb3e6nK5ndzlkTLnP3JBu4LKkbrYrU69gCVceV0JvpyuW1xlsUVngzhwMetn/XamtTORF9IO5YnWNiyeF9zCAfqR3fUW+vZZKLtgP+ts8BmQRBREAdRDhH3o8QuRh/YucNFz2BEjxbRN6LGzphfKmvP6v6QhqIQyZ8XNJ0W0X83MR1PEcJBNO2KC2Z1TW/v244scp9FwRViZxIOBF0Lctk7ZVSavdLvRlV1hz/ysUi9sr8CIcB3nvWBwA93ykTz18eAYxQ6N/K2DkPA1lv3iXCwmDUT7YkjIby9siXueIJj9H+pzSqJ9oIuJWTUgSSt4WO7o/9GGg0viR4VinNRUDoIj34xoCd6pxD3aK3zfdbnx5v1J3ZNNEJsE0sBG7N27ReDrJc4sFxz7dI/ZAbOmmiKvHBitQXpAdR6+F7v+/ol/tOouUV01EeMZQF2BoQDn6dP4XNr+j9GZEtEK1/L8pFw7bd3a53tsTa7WD+054jOFmPg1XBKPQgnqFfmFcy32ZRvjmiIIQTYFvyDxQ8nH8WIwwGwlyDjDznnilYyFr6njrlZwsKkBpO59A7OwgdzPEWRm+G+oeb7IfyNuzjEEVLrOVxJsxvxwF8kmCM6I2QYmJunz4u4TrADpfl7mlbRTWQ7VmrBzh3+C9f6Grc3YoGN9dg/SXFthpRsT6vobfXRs2VBlgBHXVMLHjDNbIZv1sZ9+X3hB09cXdH1JKViyG0+W9bWZDa/r2f9zAFR71sTzGpMSWz2iI4YssWjWo3REy1MDGjdwe5e0dFSiAC1JakBvu4/CUS8Eh6dqHdU0Or0ioY3W5ClSqDXAy7/6SRfgw8vt4I+tbvvNtFT2kVDhY5+IGb1rCqYaXNF08vSALsXCPmt0kQNqJT1p5eI1mkIV/BxCY1z85lOzeFbPBQHURkkPTlwTYK9gTVE25l84IbFFN+YJDHjdpn0gq6mrHht0dkcjbM4UL9283O5p77GN+SPW/QwVB4IUYg7Or+Kp7naR6qktP98LNF2UxWo9yObPIT9KYg+hK4i56no4rfnM0qeyFf6AwAAAP//trwR3wAAAAZJREFUAwBZ0sR75itw5gAAAABJRU5ErkJggg==";

fn assert_wall_time_line(line: &str) {
    assert_regex_match(r"^Wall time: [0-9]+(?:\.[0-9]+)? seconds$", line);
}

fn split_wall_time_wrapped_output(output: &str) -> &str {
    let (wall_time, rest) = output
        .split_once('\n')
        .expect("wall-time output should contain an Output section");
    assert_wall_time_line(wall_time);
    rest.strip_prefix("Output:\n")
        .expect("wall-time output should contain Output marker")
}

fn assert_wall_time_header(output: &str) {
    let (wall_time, marker) = output
        .split_once('\n')
        .expect("wall-time header should contain an Output marker");
    assert_wall_time_line(wall_time);
    assert_eq!(marker, "Output:");
}

fn read_only_user_turn(fixture: &TestCodex, text: impl Into<String>) -> Op {
    read_only_user_turn_with_model(fixture, text, fixture.session_configured.model.clone())
}

fn read_only_user_turn_with_model(
    fixture: &TestCodex,
    text: impl Into<String>,
    model: String,
) -> Op {
    user_turn_with_permission_profile(fixture, text, model, PermissionProfile::read_only())
}

fn auto_approved_user_turn(fixture: &TestCodex, text: impl Into<String>) -> Op {
    user_turn_with_permission_profile(
        fixture,
        text,
        fixture.session_configured.model.clone(),
        PermissionProfile::Disabled,
    )
}

fn user_turn_with_permission_profile(
    fixture: &TestCodex,
    text: impl Into<String>,
    model: String,
    permission_profile: PermissionProfile,
) -> Op {
    let cwd = fixture.config.cwd.clone();
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(permission_profile, cwd.as_path());
    Op::UserInput {
        items: vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }],
        final_output_json_schema: None,
        responsesapi_client_metadata: None,
        additional_context: Default::default(),
        thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
            approval_policy: Some(AskForApproval::Never),
            sandbox_policy: Some(sandbox_policy),
            permission_profile,
            collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
                mode: codex_protocol::config_types::ModeKind::Default,
                settings: codex_protocol::config_types::Settings {
                    model,
                    reasoning_effort: None,
                    developer_instructions: None,
                },
            }),
            ..Default::default()
        },
    }
}

#[derive(Debug, PartialEq, Eq)]
enum McpCallEvent {
    Begin(String),
    End(String),
}

fn remote_aware_environment_id() -> String {
    codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string()
}

/// Returns the native Windows stdio MCP test server command path.
fn remote_aware_stdio_server_bin() -> anyhow::Result<String> {
    Ok(stdio_server_bin()?)
}

struct TestMcpServerOptions {
    environment_id: String,
    auth: McpServerAuth,
    supports_parallel_tool_calls: bool,
    tool_timeout_sec: Option<Duration>,
}

impl Default for TestMcpServerOptions {
    fn default() -> Self {
        Self {
            environment_id: codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            auth: McpServerAuth::default(),
            supports_parallel_tool_calls: false,
            tool_timeout_sec: None,
        }
    }
}

fn stdio_transport(
    command: String,
    env: Option<HashMap<String, String>>,
    env_vars: Vec<McpServerEnvVar>,
) -> McpServerTransportConfig {
    stdio_transport_with_cwd(command, env, env_vars, /*cwd*/ None)
}

fn stdio_transport_with_cwd(
    command: String,
    env: Option<HashMap<String, String>>,
    env_vars: Vec<McpServerEnvVar>,
    cwd: Option<PathBuf>,
) -> McpServerTransportConfig {
    McpServerTransportConfig::Stdio {
        command,
        args: Vec::new(),
        env,
        env_vars,
        cwd: cwd.map(|cwd| LegacyAppPathString::from_path(&cwd)),
    }
}

fn insert_mcp_server(
    config: &mut Config,
    server_name: &str,
    transport: McpServerTransportConfig,
    options: TestMcpServerOptions,
) {
    let mut servers = config.mcp_servers.get().clone();
    servers.insert(
        server_name.to_string(),
        McpServerConfig {
            transport,
            auth: options.auth,
            environment_id: options.environment_id,
            enabled: true,
            required: false,
            supports_parallel_tool_calls: options.supports_parallel_tool_calls,
            disabled_reason: None,
            startup_timeout_sec: Some(Duration::from_secs(10)),
            tool_timeout_sec: options.tool_timeout_sec,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );
    config
        .mcp_servers
        .set(servers)
        .expect("test mcp servers should accept any configuration");
}

async fn call_cwd_tool(
    server: &MockServer,
    fixture: &TestCodex,
    server_name: &str,
    call_id: &str,
) -> anyhow::Result<Value> {
    call_structured_tool(server, fixture, server_name, "cwd", call_id).await
}

async fn call_structured_tool(
    server: &MockServer,
    fixture: &TestCodex,
    server_name: &str,
    tool_name: &str,
    call_id: &str,
) -> anyhow::Result<Value> {
    let namespace = format!("mcp__{server_name}");
    mount_sse_once(
        server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(call_id, &namespace, tool_name, r#"{}"#),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    mount_sse_once(
        server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp tool completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    fixture
        .codex
        .submit(read_only_user_turn(fixture, "call the requested rmcp tool"))
        .await?;

    wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallBegin(_))
    })
    .await;
    let end_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallEnd(_))
    })
    .await;
    let EventMsg::McpToolCallEnd(end) = end_event else {
        unreachable!("event guard guarantees McpToolCallEnd");
    };
    let structured_content = end
        .result
        .as_ref()
        .expect("rmcp tool should return success")
        .structured_content
        .as_ref()
        .expect("structured content")
        .clone();

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    Ok(structured_content)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn openai_form_capability_is_advertised_to_mcp_servers() -> anyhow::Result<()> {
    assert_openai_form_capability_advertisement(/*expected*/ true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn openai_form_capability_is_not_advertised_by_default() -> anyhow::Result<()> {
    assert_openai_form_capability_advertisement(/*expected*/ false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn openai_form_capability_updates_for_loaded_thread() -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let server_name = "capabilities";
    let command = stdio_server_bin()?;
    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(command, /*env*/ None, Vec::new()),
                TestMcpServerOptions::default(),
            );
        })
        .build(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    let unsupported = call_structured_tool(
        &server,
        &fixture,
        server_name,
        "client_capabilities",
        "call-client-capabilities-unsupported",
    )
    .await?;
    assert_eq!(
        unsupported,
        json!({ "supportsOpenaiFormElicitation": false })
    );

    fixture
        .codex
        .set_openai_form_elicitation_support(/*supported*/ true)
        .await?;
    let supported = call_structured_tool(
        &server,
        &fixture,
        server_name,
        "client_capabilities",
        "call-client-capabilities-supported",
    )
    .await?;
    assert_eq!(supported, json!({ "supportsOpenaiFormElicitation": true }));
    Ok(())
}

async fn assert_openai_form_capability_advertisement(expected: bool) -> anyhow::Result<()> {
    let server = start_mock_server().await;
    let server_name = "capabilities";
    let command = stdio_server_bin()?;
    let mut builder = test_codex().with_config(move |config| {
        insert_mcp_server(
            config,
            server_name,
            stdio_transport(command, /*env*/ None, Vec::new()),
            TestMcpServerOptions::default(),
        );
    });
    if expected {
        builder = builder.with_openai_form_elicitation();
    }
    let fixture = builder.build(&server).await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    let structured = call_structured_tool(
        &server,
        &fixture,
        server_name,
        "client_capabilities",
        "call-client-capabilities",
    )
    .await?;
    assert_eq!(
        structured,
        json!({ "supportsOpenaiFormElicitation": expected })
    );
    Ok(())
}

fn assert_cwd_tool_output(structured: &Value, expected_cwd: &Path) {
    let actual_cwd = structured
        .get("cwd")
        .and_then(Value::as_str)
        .expect("cwd tool should return a string cwd");

    // Windows can report the same absolute directory through an 8.3 path.
    // Canonical paths keep the assertion focused on cwd precedence.
    assert_eq!(
        Path::new(actual_cwd)
            .canonicalize()
            .expect("cwd tool path should exist"),
        expected_cwd
            .canonicalize()
            .expect("expected cwd should exist"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(mcp_test_value)]
async fn stdio_server_round_trip() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let call_id = "call-123";
    let search_call_id = "search-rmcp-echo";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_tool_search_call(
                search_call_id,
                &json!({"query": "echo message and environment data"}),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let call_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-2"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "echo",
                "{\"message\":\"ping\"}",
            ),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp echo tool completed successfully."),
            responses::ev_completed("resp-3"),
        ]),
    )
    .await;

    let expected_env_value = "propagated-env";
    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(
                    rmcp_test_server_bin,
                    Some(HashMap::from([(
                        "MCP_TEST_VALUE".to_string(),
                        expected_env_value.to_string(),
                    )])),
                    Vec::new(),
                ),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        .submit(read_only_user_turn(&fixture, "call the rmcp echo tool"))
        .await?;

    let begin_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallBegin(_))
    })
    .await;

    let EventMsg::McpToolCallBegin(begin) = begin_event else {
        unreachable!("event guard guarantees McpToolCallBegin");
    };
    assert_eq!(begin.invocation.server, server_name);
    assert_eq!(begin.invocation.tool, "echo");

    let end_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallEnd(_))
    })
    .await;
    let EventMsg::McpToolCallEnd(end) = end_event else {
        unreachable!("event guard guarantees McpToolCallEnd");
    };

    let result = end
        .result
        .as_ref()
        .expect("rmcp echo tool should return success");
    assert_eq!(result.is_error, Some(false));
    assert!(
        result.content.is_empty(),
        "content should default to an empty array"
    );

    let structured = result
        .structured_content
        .as_ref()
        .expect("structured content");
    let map = structured
        .as_object()
        .expect("structured content should be an object");
    let echo_value = map
        .get("echo")
        .and_then(Value::as_str)
        .expect("echo payload present");
    assert_eq!(echo_value, "ECHOING: ping");
    let env_value = map
        .get("env")
        .and_then(Value::as_str)
        .expect("env snapshot inserted");
    assert_eq!(env_value, expected_env_value);

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let search_output = call_mock
        .single_request()
        .tool_search_output(search_call_id);
    assert!(
        responses::namespace_child_tool(&search_output, &namespace, "echo").is_some(),
        "tool_search should surface the RMCP echo tool: {search_output:?}"
    );
    let output_item = final_mock.single_request().function_call_output(call_id);

    let output_text = output_item
        .get("output")
        .and_then(Value::as_str)
        .expect("function_call_output output should be a string");
    let wrapped_payload = split_wall_time_wrapped_output(output_text);
    let output_json: Value = serde_json::from_str(wrapped_payload)
        .expect("wrapped MCP output should preserve structured JSON");
    assert_eq!(output_json["echo"], "ECHOING: ping");
    assert_eq!(output_json["env"], expected_env_value);

    server.verify().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_cancels_startup_prewarm_waiting_for_mcp_startup() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_websocket_server(vec![vec![vec![
        responses::ev_response_created("warm-1"),
        responses::ev_completed("warm-1"),
    ]]])
    .await;
    let pending_mcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let pending_mcp_url = format!("http://{}/mcp", pending_mcp_listener.local_addr()?);

    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                "shutdown_prewarm",
                McpServerTransportConfig::StreamableHttp {
                    url: pending_mcp_url,
                    bearer_token_env_var: None,
                    http_headers: None,
                    env_http_headers: None,
                },
                TestMcpServerOptions::default(),
            );
        })
        .build_with_websocket_server(&server)
        .await?;

    let (_pending_mcp_connection, _) =
        tokio::time::timeout(Duration::from_secs(5), pending_mcp_listener.accept())
            .await
            .context("startup prewarm should start the MCP connection")??;
    tokio::time::timeout(Duration::from_secs(2), fixture.codex.shutdown_and_wait())
        .await
        .context("shutdown should not wait for startup prewarm MCP startup")??;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        server.connections().is_empty(),
        "startup prewarm should not send a websocket request after shutdown"
    );

    server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(mcp_cwd)]
async fn stdio_server_uses_configured_cwd_before_runtime_fallback() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let server_name = "rmcp_configured_cwd";
    let expected_cwd = Arc::new(Mutex::new(None::<PathBuf>));
    let expected_cwd_for_config = Arc::clone(&expected_cwd);
    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_workspace_setup(|cwd, fs| async move {
            let configured_cwd = cwd.join("mcp-configured-cwd");
            let configured_cwd_uri = PathUri::from_host_native_path(&configured_cwd)?;
            fs.create_directory(
                &configured_cwd_uri,
                CreateDirectoryOptions { recursive: true },
                /*sandbox*/ None,
            )
            .await?;
            Ok::<(), anyhow::Error>(())
        })
        .with_config(move |config| {
            let configured_cwd = config.cwd.join("mcp-configured-cwd").into_path_buf();
            *expected_cwd_for_config
                .lock()
                .expect("expected cwd lock should not be poisoned") = Some(configured_cwd.clone());
            insert_mcp_server(
                config,
                server_name,
                stdio_transport_with_cwd(
                    rmcp_test_server_bin,
                    Some(HashMap::from([(
                        "MCP_TEST_VALUE".to_string(),
                        "configured-cwd".to_string(),
                    )])),
                    Vec::new(),
                    Some(configured_cwd),
                ),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    let expected_cwd = expected_cwd
        .lock()
        .expect("expected cwd lock should not be poisoned")
        .clone()
        .expect("test config should record configured MCP cwd");
    let structured = call_cwd_tool(&server, &fixture, server_name, "call-configured-cwd").await?;

    assert_cwd_tool_output(&structured, &expected_cwd);
    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn stdio_mcp_tool_call_includes_sandbox_state_meta() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let call_id = "sandbox-meta-call";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(call_id, &namespace, "sandbox_meta", "{}"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp sandbox meta completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;
    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(rmcp_test_server_bin, /*env*/ None, Vec::new()),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;

    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .submit_turn_with_permission_profile(
            "call the rmcp sandbox_meta tool",
            PermissionProfile::read_only(),
        )
        .await?;

    let output_item = final_mock.single_request().function_call_output(call_id);
    let output_text = output_item
        .get("output")
        .and_then(Value::as_str)
        .expect("function_call_output output should be a string");
    let wrapped_payload = split_wall_time_wrapped_output(output_text);
    let output_json: Value = serde_json::from_str(wrapped_payload)
        .expect("wrapped MCP output should preserve sandbox metadata JSON");
    let meta = output_json
        .as_object()
        .expect("sandbox_meta should return metadata object");

    let sandbox_meta = meta
        .get(MCP_SANDBOX_STATE_META_CAPABILITY)
        .expect("sandbox state metadata should be present");
    let sandbox_state: SandboxState = serde_json::from_value(sandbox_meta.clone())?;
    assert_eq!(
        sandbox_state,
        SandboxState {
            permission_profile: PermissionProfile::read_only(),
            sandbox_cwd: PathUri::from_abs_path(&fixture.config.cwd),
        }
    );

    server.verify().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_mcp_parallel_tool_calls_default_false_runs_serially() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let first_call_id = "sync-serial-1";
    let second_call_id = "sync-serial-2";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");
    let args = json!({ "sleep_after_ms": 100 }).to_string();

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(first_call_id, &namespace, "sync", &args),
            responses::ev_function_call_with_namespace(second_call_id, &namespace, "sync", &args),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp sync tools completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(rmcp_test_server_bin, /*env*/ None, Vec::new()),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    tool_timeout_sec: Some(Duration::from_secs(2)),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        // Keep this baseline on the mutable sync tool so read-only hints do not
        // make the call parallel-safe. Bypass read-only turn permissions so
        // approval behavior does not block the scheduling assertion.
        .submit(auto_approved_user_turn(
            &fixture,
            "call the rmcp sync tool twice",
        ))
        .await?;

    let mut call_events = Vec::new();
    while call_events.len() < 4 {
        let event = wait_for_event(&fixture.codex, |ev| {
            matches!(
                ev,
                EventMsg::McpToolCallBegin(_) | EventMsg::McpToolCallEnd(_)
            )
        })
        .await;
        match event {
            EventMsg::McpToolCallBegin(begin) => {
                call_events.push(McpCallEvent::Begin(begin.call_id));
            }
            EventMsg::McpToolCallEnd(end) => {
                call_events.push(McpCallEvent::End(end.call_id));
            }
            _ => unreachable!("event guard guarantees MCP call events"),
        }
    }

    let event_index = |needle: McpCallEvent| {
        call_events
            .iter()
            .position(|event| event == &needle)
            .expect("expected MCP call event")
    };
    let first_begin = event_index(McpCallEvent::Begin(first_call_id.to_string()));
    let first_end = event_index(McpCallEvent::End(first_call_id.to_string()));
    let second_begin = event_index(McpCallEvent::Begin(second_call_id.to_string()));
    let second_end = event_index(McpCallEvent::End(second_call_id.to_string()));
    assert!(
        first_end < second_begin || second_end < first_begin,
        "default MCP tool calls should run serially; saw events: {call_events:?}"
    );

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = final_mock.single_request();
    for call_id in [first_call_id, second_call_id] {
        let output_text = request
            .function_call_output_text(call_id)
            .expect("function_call_output present for rmcp sync call");
        let wrapped_payload = split_wall_time_wrapped_output(&output_text);
        let output_json: Value = serde_json::from_str(wrapped_payload)
            .expect("wrapped MCP output should preserve structured JSON");
        assert_eq!(output_json, json!({ "result": "ok" }));
    }

    server.verify().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_mcp_read_only_tool_calls_run_concurrently_without_server_opt_in()
-> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let first_call_id = "sync-read-only-1";
    let second_call_id = "sync-read-only-2";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");
    // The stdio MCP test server holds each sync call at this barrier until both
    // calls arrive. A serial scheduler times out inside the server instead of
    // returning the structured `{ "result": "ok" }` result asserted below.
    let args = json!({
        "sleep_after_ms": 100,
        "barrier": {
            "id": "stdio-mcp-read-only-tool-calls",
            "participants": 2,
            "timeout_ms": 1_000
        }
    })
    .to_string();

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                first_call_id,
                &namespace,
                "sync_readonly",
                &args,
            ),
            responses::ev_function_call_with_namespace(
                second_call_id,
                &namespace,
                "sync_readonly",
                &args,
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp sync tools completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(rmcp_test_server_bin, /*env*/ None, Vec::new()),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    tool_timeout_sec: Some(Duration::from_secs(2)),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        .submit(read_only_user_turn(
            &fixture,
            "call the rmcp sync_readonly tool twice",
        ))
        .await?;

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = final_mock.single_request();
    for call_id in [first_call_id, second_call_id] {
        let output_text = request
            .function_call_output_text(call_id)
            .expect("function_call_output present for rmcp sync call");
        let wrapped_payload = split_wall_time_wrapped_output(&output_text);
        let output_json: Value = serde_json::from_str(wrapped_payload)
            .expect("wrapped MCP output should preserve structured JSON");
        assert_eq!(output_json, json!({ "result": "ok" }));
    }

    server.verify().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_mcp_parallel_tool_calls_opt_in_runs_concurrently() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let first_call_id = "sync-1";
    let second_call_id = "sync-2";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");
    let args = json!({
        "sleep_after_ms": 100,
        "barrier": {
            "id": "stdio-mcp-parallel-tool-calls",
            "participants": 2,
            "timeout_ms": 1_000
        }
    })
    .to_string();

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(first_call_id, &namespace, "sync", &args),
            responses::ev_function_call_with_namespace(second_call_id, &namespace, "sync", &args),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp sync tools completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(rmcp_test_server_bin, /*env*/ None, Vec::new()),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    auth: Default::default(),
                    supports_parallel_tool_calls: true,
                    tool_timeout_sec: Some(Duration::from_secs(2)),
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        // Exercise the server opt-in with the mutable sync tool rather than the
        // read-only sync_readonly tool. Bypass read-only turn permissions so
        // approval behavior does not block the scheduling assertion.
        .submit(auto_approved_user_turn(
            &fixture,
            "call the rmcp sync tool twice",
        ))
        .await?;

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = final_mock.single_request();
    for call_id in [first_call_id, second_call_id] {
        let output_text = request
            .function_call_output_text(call_id)
            .expect("function_call_output present for rmcp sync call");
        let wrapped_payload = split_wall_time_wrapped_output(&output_text);
        let output_json: Value = serde_json::from_str(wrapped_payload)
            .expect("wrapped MCP output should preserve structured JSON");
        assert_eq!(output_json, json!({ "result": "ok" }));
    }

    server.verify().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(mcp_test_value)]
async fn stdio_image_responses_round_trip() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let call_id = "img-1";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");

    // First stream: model decides to call the image tool.
    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(call_id, &namespace, "image", "{}"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    // Second stream: after tool execution, assistant emits a message and completes.
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp image tool completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    // Build the stdio rmcp server and pass the image as data URL so it can construct ImageContent.
    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_model("gpt-5.2")
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(
                    rmcp_test_server_bin,
                    Some(HashMap::from([(
                        "MCP_TEST_IMAGE_DATA_URL".to_string(),
                        OPENAI_PNG.to_string(),
                    )])),
                    Vec::new(),
                ),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        .submit(read_only_user_turn(&fixture, "call the rmcp image tool"))
        .await?;

    // Wait for tool begin/end and final completion.
    let begin_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallBegin(_))
    })
    .await;
    let EventMsg::McpToolCallBegin(begin) = begin_event else {
        unreachable!("begin");
    };
    assert_eq!(
        begin,
        McpToolCallBeginEvent {
            call_id: call_id.to_string(),
            invocation: McpInvocation {
                server: server_name.to_string(),
                tool: "image".to_string(),
                arguments: Some(json!({})),
            },
            connector_id: None,
            mcp_app_resource_uri: None,
            link_id: None,
            app_name: None,
            template_id: None,
            action_name: None,
            plugin_id: None,
        },
    );

    let end_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallEnd(_))
    })
    .await;
    let EventMsg::McpToolCallEnd(end) = end_event else {
        unreachable!("end");
    };
    assert_eq!(end.call_id, call_id);
    assert_eq!(
        end.invocation,
        McpInvocation {
            server: server_name.to_string(),
            tool: "image".to_string(),
            arguments: Some(json!({})),
        }
    );
    let result = end.result.expect("rmcp image tool should return success");
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
    let base64_only = OPENAI_PNG
        .strip_prefix("data:image/png;base64,")
        .expect("data url prefix");
    let entry = result.content[0].as_object().expect("content object");
    assert_eq!(entry.get("type"), Some(&json!("image")));
    assert_eq!(entry.get("mimeType"), Some(&json!("image/png")));
    assert_eq!(entry.get("data"), Some(&json!(base64_only)));

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let output_item = final_mock.single_request().function_call_output(call_id);
    assert_eq!(output_item["type"], "function_call_output");
    assert_eq!(output_item["call_id"], call_id);
    let output = output_item["output"]
        .as_array()
        .expect("image MCP output should be content items");
    assert_eq!(output.len(), 2);
    assert_wall_time_header(
        output[0]["text"]
            .as_str()
            .expect("first MCP image output item should be wall-time text"),
    );
    assert_eq!(
        output[1],
        json!({
            "type": "input_image",
            "image_url": OPENAI_PNG,
            "detail": "high"
        })
    );
    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(mcp_test_value)]
async fn stdio_image_responses_resize_large_image() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let call_id = "img-resize-1";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");

    // Keep the source wider than the model limit while avoiding a multi-million-pixel
    // fixture that can exhaust the event wait budget in an unoptimized test build.
    let original_dimensions = (2400, 100);
    let image = ImageBuffer::from_pixel(
        original_dimensions.0,
        original_dimensions.1,
        Rgba([20, 40, 60, 255]),
    );
    let mut encoded = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image).write_to(&mut encoded, image::ImageFormat::Png)?;
    let image_data_url = format!(
        "data:image/png;base64,{}",
        BASE64_STANDARD.encode(encoded.into_inner())
    );
    let tool_arguments = serde_json::to_string(&json!({
        "scenario": "image_only",
        "data_url": image_data_url,
    }))?;

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "image_scenario",
                &tool_arguments,
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "done"),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;
    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(rmcp_test_server_bin, /*env*/ None, Vec::new()),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        .submit(read_only_user_turn(
            &fixture,
            "call the rmcp image_scenario tool",
        ))
        .await?;
    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let output_item = final_mock.single_request().function_call_output(call_id);
    assert_eq!(output_item["call_id"], call_id);
    let output = output_item["output"]
        .as_array()
        .expect("image MCP output should be content items");
    let resized_url = output[1]["image_url"]
        .as_str()
        .expect("MCP image output should contain a data URL");
    assert_eq!(output[1]["detail"], "high");
    let (_, resized_base64) = resized_url
        .split_once(',')
        .expect("resized image should contain a data URL prefix");
    let resized_bytes = BASE64_STANDARD.decode(resized_base64)?;
    let resized = image::load_from_memory(&resized_bytes)?;
    let resized_dimensions = resized.dimensions();
    assert_eq!(resized_dimensions, (2048, 85));

    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(mcp_test_value)]
async fn stdio_image_responses_preserve_original_detail_metadata() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let call_id = "img-original-detail-1";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "image_scenario",
                r#"{"scenario":"image_only_original_detail"}"#,
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp original-detail image completed."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_model("gpt-5.4")
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(rmcp_test_server_bin, /*env*/ None, Vec::new()),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        .submit(read_only_user_turn(
            &fixture,
            "call the rmcp image_scenario tool",
        ))
        .await?;

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let output_item = final_mock.single_request().function_call_output(call_id);
    let output = output_item["output"]
        .as_array()
        .expect("image MCP output should be content items");
    assert_eq!(output.len(), 2);
    assert_wall_time_header(
        output[0]["text"]
            .as_str()
            .expect("first MCP image output item should be wall-time text"),
    );
    assert_eq!(
        output[1],
        json!({
            "type": "input_image",
            "image_url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==",
            "detail": "original",
        })
    );

    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(mcp_test_value)]
async fn stdio_image_responses_are_sanitized_for_text_only_model() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let call_id = "img-text-only-1";
    let server_name = "rmcp";
    let namespace = format!("mcp__{server_name}");
    let text_only_model_slug = "rmcp-text-only-model";

    let models_mock = mount_models_once(
        &server,
        ModelsResponse {
            models: vec![ModelInfo {
                slug: text_only_model_slug.to_string(),
                display_name: "RMCP Text Only".to_string(),
                description: Some("Test model without image input support".to_string()),
                default_reasoning_level: None,
                supported_reasoning_levels: vec![ReasoningEffortPreset {
                    effort: codex_protocol::openai_models::ReasoningEffort::Medium,
                    description: "Medium".to_string(),
                }],
                shell_type: ConfigShellToolType::Default,
                visibility: ModelVisibility::List,
                supported_in_api: true,
                priority: 1,
                additional_speed_tiers: Vec::new(),
                service_tiers: Vec::new(),
                default_service_tier: None,
                upgrade: None,
                base_instructions: "base instructions".to_string(),
                model_messages: None,
                include_skills_usage_instructions: false,
                supports_reasoning_summaries: false,
                default_reasoning_summary: ReasoningSummary::Auto,
                support_verbosity: false,
                default_verbosity: None,
                availability_nux: None,
                apply_patch_tool_type: None,
                web_search_tool_type: Default::default(),
                truncation_policy: TruncationPolicyConfig::bytes(/*limit*/ 10_000),
                supports_parallel_tool_calls: false,
                supports_image_detail_original: false,
                context_window: Some(272_000),
                max_context_window: None,
                auto_compact_token_limit: None,
                comp_hash: None,
                effective_context_window_percent: 95,
                experimental_supported_tools: Vec::new(),
                input_modalities: vec![InputModality::Text],
                used_fallback_model_metadata: false,
                supports_search_tool: false,
                use_responses_lite: false,
                auto_review_model_override: None,
                tool_mode: None,
                multi_agent_version: None,
            }],
        },
    )
    .await;

    // First stream: model decides to call the image tool.
    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(call_id, &namespace, "image", "{}"),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    // Second stream: after tool execution, assistant emits a message and completes.
    let final_mock = mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp image tool completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(
                    rmcp_test_server_bin,
                    Some(HashMap::from([(
                        "MCP_TEST_IMAGE_DATA_URL".to_string(),
                        OPENAI_PNG.to_string(),
                    )])),
                    Vec::new(),
                ),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .thread_manager
        .get_models_manager()
        .list_models(
            RefreshStrategy::Online,
            codex_core::test_support::default_http_client_factory(),
        )
        .await
        .expect("model listing should succeed");
    assert_eq!(models_mock.requests().len(), 1);

    fixture
        .codex
        .submit(read_only_user_turn_with_model(
            &fixture,
            "call the rmcp image tool",
            text_only_model_slug.to_string(),
        ))
        .await?;

    wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallBegin(_))
    })
    .await;
    wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallEnd(_))
    })
    .await;
    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let output_item = final_mock.single_request().function_call_output(call_id);
    let output_text = output_item
        .get("output")
        .and_then(Value::as_str)
        .expect("function_call_output output should be a JSON string");
    let wrapped_payload = split_wall_time_wrapped_output(output_text);
    let output_json: Value = serde_json::from_str(wrapped_payload)
        .expect("function_call_output output should be valid JSON");
    assert_eq!(
        output_json,
        json!([{
            "type": "text",
            "text": "<image content omitted because you do not support image input>"
        }])
    );
    server.verify().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(mcp_test_value)]
async fn stdio_server_propagates_whitelisted_env_vars() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;

    let call_id = "call-1234";
    let server_name = "rmcp_whitelist";
    let namespace = format!("mcp__{server_name}");

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "echo",
                "{\"message\":\"ping\"}",
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp echo tool completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let expected_env_value = "propagated-env-from-whitelist";
    let _guard = EnvVarGuard::set("MCP_TEST_VALUE", OsStr::new(expected_env_value));
    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(
                    rmcp_test_server_bin,
                    /*env*/ None,
                    vec!["MCP_TEST_VALUE".into()],
                ),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        .submit(read_only_user_turn(&fixture, "call the rmcp echo tool"))
        .await?;

    let begin_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallBegin(_))
    })
    .await;

    let EventMsg::McpToolCallBegin(begin) = begin_event else {
        unreachable!("event guard guarantees McpToolCallBegin");
    };
    assert_eq!(begin.invocation.server, server_name);
    assert_eq!(begin.invocation.tool, "echo");

    let end_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallEnd(_))
    })
    .await;
    let EventMsg::McpToolCallEnd(end) = end_event else {
        unreachable!("event guard guarantees McpToolCallEnd");
    };

    let result = end
        .result
        .as_ref()
        .expect("rmcp echo tool should return success");
    assert_eq!(result.is_error, Some(false));
    assert!(
        result.content.is_empty(),
        "content should default to an empty array"
    );

    let structured = result
        .structured_content
        .as_ref()
        .expect("structured content");
    let map = structured
        .as_object()
        .expect("structured content should be an object");
    let echo_value = map
        .get("echo")
        .and_then(Value::as_str)
        .expect("echo payload present");
    assert_eq!(echo_value, "ECHOING: ping");
    let env_value = map
        .get("env")
        .and_then(Value::as_str)
        .expect("env snapshot inserted");
    assert_eq!(env_value, expected_env_value);

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    server.verify().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
#[serial(mcp_env_source)]
async fn stdio_server_propagates_explicit_local_env_var_source() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let call_id = "call-local-source";
    let server_name = "rmcp_local_source";
    let namespace = format!("mcp__{server_name}");
    let env_name = "MCP_TEST_LOCAL_SOURCE";
    let expected_env_value = "propagated-explicit-local-source";

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "echo",
                &format!(r#"{{"message":"ping","env_var":"{env_name}"}}"#),
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("msg-1", "rmcp echo tool completed successfully."),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    let _guard = EnvVarGuard::set(env_name, OsStr::new(expected_env_value));
    let rmcp_test_server_bin = remote_aware_stdio_server_bin()?;

    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                stdio_transport(
                    rmcp_test_server_bin,
                    /*env*/ None,
                    vec![McpServerEnvVar::Config {
                        name: env_name.to_string(),
                        source: Some("local".to_string()),
                    }],
                ),
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    fixture
        .codex
        .submit(read_only_user_turn(&fixture, "call the rmcp echo tool"))
        .await?;

    wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallBegin(_))
    })
    .await;
    let end_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallEnd(_))
    })
    .await;
    let EventMsg::McpToolCallEnd(end) = end_event else {
        unreachable!("event guard guarantees McpToolCallEnd");
    };
    let structured = end
        .result
        .as_ref()
        .expect("rmcp echo tool should return success")
        .structured_content
        .as_ref()
        .expect("structured content");
    assert_eq!(structured["env"], expected_env_value);

    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;
    server.verify().await;
    Ok(())
}

/// OAuth metadata path served by the Streamable HTTP MCP test server.
const STREAMABLE_HTTP_METADATA_PATH: &str = "/.well-known/oauth-authorization-server/mcp";

/// Streamable HTTP test server plus the process handle needed for cleanup.
struct StreamableHttpTestServer {
    server_url: String,
    process: Child,
}

impl StreamableHttpTestServer {
    /// Returns the MCP endpoint URL that Codex should connect to.
    fn url(&self) -> &str {
        &self.server_url
    }

    /// Stops the native Windows test server and waits for process exit.
    async fn shutdown(mut self) {
        match self.process.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) => {
                let _ = self.process.kill().await;
            }
            Err(error) => {
                eprintln!("failed to check streamable http server status: {error}");
                let _ = self.process.kill().await;
            }
        };
        if let Err(error) = self.process.wait().await {
            eprintln!("failed to await streamable http server shutdown: {error}");
        }
    }
}

/// What this tests: Codex can discover and call a Streamable HTTP MCP tool from
/// the native Windows test environment, preserving the server environment.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_tool_call_round_trip() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    // Phase 1: script the model responses so Codex will call the MCP echo tool
    // and then complete the turn after the tool result is returned.
    let server = responses::start_mock_server().await;

    let call_id = "call-456";
    let server_name = "rmcp_http";
    let namespace = format!("mcp__{server_name}");

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "echo",
                "{\"message\":\"ping\"}",
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message(
                "msg-1",
                "rmcp streamable http echo tool completed successfully.",
            ),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    // Phase 2: start the Streamable HTTP MCP test server as a local process.
    let expected_env_value = "propagated-env-http";
    let http_server =
        start_streamable_http_test_server(expected_env_value, /*expected_token*/ None).await?;
    let server_url = http_server.url().to_string();

    // Phase 3: configure Codex with the Streamable HTTP MCP server.
    let fixture = test_codex()
        .with_config(move |config| {
            insert_mcp_server(
                config,
                server_name,
                McpServerTransportConfig::StreamableHttp {
                    url: server_url,
                    bearer_token_env_var: None,
                    http_headers: None,
                    env_http_headers: None,
                },
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    // Phase 4: submit the user turn that should trigger the MCP tool call.
    fixture
        .codex
        .submit(read_only_user_turn(
            &fixture,
            "call the rmcp streamable http echo tool",
        ))
        .await?;

    // Phase 5: assert Codex begins the expected tool invocation.
    let begin_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallBegin(_))
    })
    .await;

    let EventMsg::McpToolCallBegin(begin) = begin_event else {
        unreachable!("event guard guarantees McpToolCallBegin");
    };
    assert_eq!(begin.invocation.server, server_name);
    assert_eq!(begin.invocation.tool, "echo");

    // Phase 6: assert the tool result proves the server handled the request and
    // propagated the expected environment value.
    let end_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallEnd(_))
    })
    .await;
    let EventMsg::McpToolCallEnd(end) = end_event else {
        unreachable!("event guard guarantees McpToolCallEnd");
    };

    let result = end
        .result
        .as_ref()
        .expect("rmcp echo tool should return success");
    assert_eq!(result.is_error, Some(false));
    assert!(
        result.content.is_empty(),
        "content should default to an empty array"
    );

    let structured = result
        .structured_content
        .as_ref()
        .expect("structured content");
    let map = structured
        .as_object()
        .expect("structured content should be an object");
    let echo_value = map
        .get("echo")
        .and_then(Value::as_str)
        .expect("echo payload present");
    assert_eq!(echo_value, "ECHOING: ping");
    let env_value = map
        .get("env")
        .and_then(Value::as_str)
        .expect("env snapshot inserted");
    assert_eq!(env_value, expected_env_value);

    // Phase 7: verify the scripted model calls were consumed and clean up the
    // placement-aware MCP server.
    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    server.verify().await;

    http_server.shutdown().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_configured_auth_precedes_chatgpt_auth() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let configured_auth_server =
        start_streamable_http_test_server("configured-auth", Some("configured-token")).await?;
    let configured_auth_url = configured_auth_server.url().to_string();

    let configured_auth_fixture = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            insert_mcp_server(
                config,
                "configured_auth",
                McpServerTransportConfig::StreamableHttp {
                    url: configured_auth_url,
                    bearer_token_env_var: None,
                    http_headers: Some(HashMap::from([(
                        "Authorization".to_string(),
                        "Bearer configured-token".to_string(),
                    )])),
                    env_http_headers: None,
                },
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    auth: McpServerAuth::ChatGpt,
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;

    wait_for_mcp_server(&configured_auth_fixture.codex, "configured_auth").await?;
    drop(configured_auth_fixture);
    configured_auth_server.shutdown().await;

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn streamable_http_chatgpt_auth_is_not_sent_to_configured_origin() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let untrusted_server = MockServer::start().await;
    let untrusted_apps = AppsTestServer::mount(&untrusted_server).await?;
    let untrusted_mcp_url = format!("{}/api/codex/apps", untrusted_apps.chatgpt_base_url);
    let untrusted_chatgpt_base_url = untrusted_apps.chatgpt_base_url;

    let fixture = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            config.chatgpt_base_url = untrusted_chatgpt_base_url;
            insert_mcp_server(
                config,
                "untrusted_origin",
                McpServerTransportConfig::StreamableHttp {
                    url: untrusted_mcp_url,
                    bearer_token_env_var: None,
                    http_headers: None,
                    env_http_headers: None,
                },
                TestMcpServerOptions {
                    auth: McpServerAuth::ChatGpt,
                    ..Default::default()
                },
            );
        })
        .build(&server)
        .await?;

    wait_for_mcp_server(&fixture.codex, "untrusted_origin").await?;
    let observed_requests = untrusted_server
        .received_requests()
        .await
        .expect("mock server should capture MCP startup requests")
        .into_iter()
        .filter(|request| request.url.path() == "/api/codex/apps")
        .filter_map(|request| {
            let body: Value = serde_json::from_slice(&request.body).ok()?;
            let method = body.get("method")?.as_str()?.to_string();
            let authorization = request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            Some((method, authorization))
        })
        .collect::<Vec<_>>();

    assert_eq!(
        observed_requests,
        vec![
            ("initialize".to_string(), None),
            ("notifications/initialized".to_string(), None),
            ("tools/list".to_string(), None),
        ],
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn configured_chatgpt_base_url_does_not_grant_mcp_chatgpt_auth() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let untrusted_server = MockServer::start().await;
    let untrusted_apps = AppsTestServer::mount(&untrusted_server).await?;
    let untrusted_mcp_url = format!("{}/api/codex/apps", untrusted_apps.chatgpt_base_url);
    let untrusted_chatgpt_base_url = untrusted_apps.chatgpt_base_url;

    let fixture = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_pre_build_hook(move |codex_home| {
            fs::write(
                codex_home.join("config.toml"),
                format!(
                    r#"
chatgpt_base_url = "{untrusted_chatgpt_base_url}"

[mcp_servers.untrusted_origin]
url = "{untrusted_mcp_url}"
auth = "chatgpt"
"#,
                ),
            )
            .expect("write attacker-controlled MCP config");
        })
        .build(&server)
        .await?;

    wait_for_mcp_server(&fixture.codex, "untrusted_origin").await?;
    let observed_requests = untrusted_server
        .received_requests()
        .await
        .expect("mock server should capture MCP startup requests")
        .into_iter()
        .filter(|request| request.url.path() == "/api/codex/apps")
        .filter_map(|request| {
            let body: Value = serde_json::from_slice(&request.body).ok()?;
            let method = body.get("method")?.as_str()?.to_string();
            let authorization = request
                .headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            Some((method, authorization))
        })
        .collect::<Vec<_>>();

    assert_eq!(
        observed_requests,
        vec![
            ("initialize".to_string(), None),
            ("notifications/initialized".to_string(), None),
            ("tools/list".to_string(), None),
        ],
    );

    Ok(())
}

/// This test writes to a fallback credentials file in CODEX_HOME.
/// Ideally, we wouldn't need to serialize the test but it's much more cumbersome to wire CODEX_HOME through the code.
#[test]
#[serial(codex_home)]
fn streamable_http_with_oauth_round_trip() -> anyhow::Result<()> {
    const TEST_STACK_SIZE_BYTES: usize = 8 * 1024 * 1024;

    let handle = std::thread::Builder::new()
        .name("streamable_http_with_oauth_round_trip".to_string())
        .stack_size(TEST_STACK_SIZE_BYTES)
        .spawn(|| -> anyhow::Result<()> {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?;
            runtime.block_on(streamable_http_with_oauth_round_trip_impl())
        })?;

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!(
            "streamable_http_with_oauth_round_trip thread panicked"
        )),
    }
}

async fn streamable_http_with_oauth_round_trip_impl() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    // Phase 1: script the model responses so Codex will call the OAuth-backed
    // MCP echo tool and then finish the turn after receiving the result.
    let server = responses::start_mock_server().await;

    let call_id = "call-789";
    let server_name = "rmcp_http_oauth";
    let namespace = format!("mcp__{server_name}");

    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("resp-1"),
            responses::ev_function_call_with_namespace(
                call_id,
                &namespace,
                "echo",
                "{\"message\":\"ping\"}",
            ),
            responses::ev_completed("resp-1"),
        ]),
    )
    .await;
    mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message(
                "msg-1",
                "rmcp streamable http oauth echo tool completed successfully.",
            ),
            responses::ev_completed("resp-2"),
        ]),
    )
    .await;

    // Phase 2: start the Streamable HTTP MCP test server with bearer-token
    // enforcement enabled so the client must use stored OAuth credentials.
    let expected_env_value = "propagated-env-http-oauth";
    let expected_token = "initial-access-token";
    let client_id = "test-client-id";
    let refresh_token = "initial-refresh-token";
    let http_server =
        start_streamable_http_test_server(expected_env_value, Some(expected_token)).await?;
    let server_url = http_server.url().to_string();

    // Phase 3: seed an isolated CODEX_HOME with fallback OAuth tokens for this
    // server so the test does not share credentials with other suite cases.
    let temp_home = Arc::new(tempdir()?);
    let _codex_home_guard = EnvVarGuard::set("CODEX_HOME", temp_home.path().as_os_str());
    write_fallback_oauth_tokens(
        temp_home.path(),
        server_name,
        &server_url,
        client_id,
        expected_token,
        refresh_token,
    )?;

    // Phase 4: configure Codex with the OAuth-backed Streamable HTTP MCP
    // server in the native Windows environment.
    let fixture = test_codex()
        .with_home(temp_home.clone())
        .with_config(move |config| {
            // Keep OAuth credentials isolated to this test home because test
            // runners may execute the full core suite in one process.
            config.mcp_oauth_credentials_store_mode = serde_json::from_value(json!("file"))
                .expect("`file` should deserialize as OAuthCredentialsStoreMode");
            insert_mcp_server(
                config,
                server_name,
                McpServerTransportConfig::StreamableHttp {
                    url: server_url,
                    bearer_token_env_var: None,
                    http_headers: None,
                    env_http_headers: None,
                },
                TestMcpServerOptions {
                    environment_id: remote_aware_environment_id(),
                    ..Default::default()
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    // Phase 5: wait for MCP startup before the turn is submitted, which keeps
    // failures tied to server startup/discovery.
    wait_for_mcp_server(&fixture.codex, server_name).await?;

    // Phase 6: submit the user turn that should invoke the OAuth-backed tool.
    fixture
        .codex
        .submit(read_only_user_turn(
            &fixture,
            "call the rmcp streamable http oauth echo tool",
        ))
        .await?;

    // Phase 7: assert Codex begins the expected tool invocation.
    let begin_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallBegin(_))
    })
    .await;

    let EventMsg::McpToolCallBegin(begin) = begin_event else {
        unreachable!("event guard guarantees McpToolCallBegin");
    };
    assert_eq!(begin.invocation.server, server_name);
    assert_eq!(begin.invocation.tool, "echo");

    // Phase 8: assert the tool result proves the authenticated request reached
    // the server and preserved the expected environment value.
    let end_event = wait_for_event(&fixture.codex, |ev| {
        matches!(ev, EventMsg::McpToolCallEnd(_))
    })
    .await;
    let EventMsg::McpToolCallEnd(end) = end_event else {
        unreachable!("event guard guarantees McpToolCallEnd");
    };

    let result = end
        .result
        .as_ref()
        .expect("rmcp echo tool should return success");
    assert_eq!(result.is_error, Some(false));
    assert!(
        result.content.is_empty(),
        "content should default to an empty array"
    );

    let structured = result
        .structured_content
        .as_ref()
        .expect("structured content");
    let map = structured
        .as_object()
        .expect("structured content should be an object");
    let echo_value = map
        .get("echo")
        .and_then(Value::as_str)
        .expect("echo payload present");
    assert_eq!(echo_value, "ECHOING: ping");
    let env_value = map
        .get("env")
        .and_then(Value::as_str)
        .expect("env snapshot inserted");
    assert_eq!(env_value, expected_env_value);

    // Phase 9: verify the scripted model calls were consumed and clean up.
    wait_for_event(&fixture.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    server.verify().await;

    http_server.shutdown().await;

    Ok(())
}

/// Starts the Streamable HTTP MCP test server as a native Windows process.
async fn start_streamable_http_test_server(
    expected_env_value: &str,
    expected_token: Option<&str>,
) -> anyhow::Result<StreamableHttpTestServer> {
    let rmcp_http_server_bin = required_streamable_http_server_bin()?;

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let bind_addr = format!("127.0.0.1:{port}");
    let server_url = format!("http://{bind_addr}/mcp");

    let mut command = Command::new(&rmcp_http_server_bin);
    command
        .kill_on_drop(true)
        .env("MCP_STREAMABLE_HTTP_BIND_ADDR", &bind_addr)
        .env("MCP_TEST_VALUE", expected_env_value);
    if let Some(expected_token) = expected_token {
        command.env("MCP_EXPECT_BEARER", expected_token);
    }
    let mut child = command.spawn()?;

    wait_for_local_streamable_http_server(&mut child, &server_url, Duration::from_secs(5)).await?;
    Ok(StreamableHttpTestServer {
        server_url,
        process: child,
    })
}

fn required_streamable_http_server_bin() -> anyhow::Result<PathBuf> {
    required_streamable_http_server_bin_with(cargo_bin)
}

fn required_streamable_http_server_bin_with(
    resolver: impl FnOnce(&str) -> Result<PathBuf, codex_utils_cargo_bin::CargoBinError>,
) -> anyhow::Result<PathBuf> {
    resolver("test_streamable_http_server")
        .context("resolve required test_streamable_http_server helper")
}

#[test]
fn missing_streamable_http_helper_resolution_reports_an_error() {
    let error = required_streamable_http_server_bin_with(|name| {
        Err(codex_utils_cargo_bin::CargoBinError::NotFound {
            name: name.to_string(),
            env_keys: vec![format!("CARGO_BIN_EXE_{name}")],
            fallback: "disabled in propagation test".to_string(),
        })
    })
    .expect_err("a missing required helper must fail the test");

    assert!(
        error
            .to_string()
            .contains("resolve required test_streamable_http_server helper")
    );
}

/// Waits for the local Streamable HTTP test server to publish OAuth metadata.
async fn wait_for_local_streamable_http_server(
    server_child: &mut Child,
    server_url: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let metadata_url = streamable_http_metadata_url(server_url);
    let client = HttpClientBuilder::new().build_direct()?;
    loop {
        if let Some(status) = server_child.try_wait()? {
            return Err(anyhow::anyhow!(
                "streamable HTTP server exited early with status {status}"
            ));
        }

        let remaining = deadline.saturating_duration_since(Instant::now());

        if remaining.is_zero() {
            return Err(anyhow::anyhow!(
                "timed out waiting for streamable HTTP server metadata at {metadata_url}: deadline reached"
            ));
        }

        match tokio::time::timeout(remaining, client.get(&metadata_url).send()).await {
            Ok(Ok(response)) if response.status() == StatusCode::OK => return Ok(()),
            Ok(Ok(response)) => {
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "timed out waiting for streamable HTTP server metadata at {metadata_url}: HTTP {}",
                        response.status()
                    ));
                }
            }
            Ok(Err(error)) => {
                if Instant::now() >= deadline {
                    return Err(anyhow::anyhow!(
                        "timed out waiting for streamable HTTP server metadata at {metadata_url}: {error}"
                    ));
                }
            }
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "timed out waiting for streamable HTTP server metadata at {metadata_url}: request timed out"
                ));
            }
        }

        sleep(Duration::from_millis(50)).await;
    }
}

/// Builds the OAuth metadata URL for the test Streamable HTTP MCP endpoint.
fn streamable_http_metadata_url(server_url: &str) -> String {
    let base_url = server_url.strip_suffix("/mcp").unwrap_or(server_url);
    format!("{base_url}{STREAMABLE_HTTP_METADATA_PATH}")
}

fn write_fallback_oauth_tokens(
    home: &Path,
    server_name: &str,
    server_url: &str,
    client_id: &str,
    access_token: &str,
    refresh_token: &str,
) -> anyhow::Result<()> {
    let expires_at = SystemTime::now()
        .checked_add(Duration::from_secs(3600))
        .ok_or_else(|| anyhow::anyhow!("failed to compute expiry time"))?
        .duration_since(UNIX_EPOCH)?
        .as_millis() as u64;

    let store = serde_json::json!({
        "stub": {
            "server_name": server_name,
            "server_url": server_url,
            "client_id": client_id,
            "access_token": access_token,
            "expires_at": expires_at,
            "refresh_token": refresh_token,
            "scopes": ["profile"],
        }
    });

    let file_path = home.join(".credentials.json");
    fs::write(&file_path, serde_json::to_vec(&store)?)?;
    Ok(())
}

struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}
