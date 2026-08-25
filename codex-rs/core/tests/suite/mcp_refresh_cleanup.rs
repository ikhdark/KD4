use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use core_test_support::process::process_is_alive;
use core_test_support::process::wait_for_pid_file;
use core_test_support::process::wait_for_process_exit;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_mcp_server;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_generation_reuses_one_mcp_process_for_100_calls() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let server = responses::start_mock_server().await;
    let temp_dir = tempfile::tempdir()?;
    let pid_file = temp_dir.path().join("mcp.pid");
    let pid_file_for_config = pid_file.clone();
    let command = stdio_server_bin()?;
    let fixture = test_codex()
        .with_config(move |config| {
            let mut servers = config.mcp_servers.get().clone();
            servers.insert(
                "stable_generation".to_string(),
                McpServerConfig {
                    auth: Default::default(),
                    transport: McpServerTransportConfig::Stdio {
                        command,
                        args: Vec::new(),
                        env: Some(HashMap::from([(
                            "MCP_TEST_PID_FILE".to_string(),
                            pid_file_for_config.to_string_lossy().into_owned(),
                        )])),
                        env_vars: Vec::new(),
                        cwd: None,
                    },
                    environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                    enabled: true,
                    required: false,
                    supports_parallel_tool_calls: false,
                    disabled_reason: None,
                    startup_timeout_sec: Some(Duration::from_secs(10)),
                    tool_timeout_sec: None,
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
                .expect("test MCP servers should accept any configuration");
        })
        .build(&server)
        .await?;
    wait_for_mcp_server(&fixture.codex, "stable_generation").await?;

    let original_pid = wait_for_pid_file(&pid_file).await?;
    fs::remove_file(&pid_file)?;
    for call in 0..100 {
        fixture
            .codex
            .call_mcp_tool(
                "stable_generation",
                "echo",
                Some(serde_json::json!({ "message": format!("call-{call}") })),
                /*meta*/ None,
            )
            .await?;
    }

    assert!(process_is_alive(&original_pid)?);
    assert!(
        !pid_file.exists(),
        "unchanged generation must not launch a replacement MCP process"
    );
    fixture.codex.shutdown_and_wait().await?;
    wait_for_process_exit(&original_pid).await
}
