use anyhow::Result;
use codex_config::types::McpServerTransportConfig;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_core::config::load_global_mcp_servers;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::Value as JsonValue;
use serde_json::json;
use tempfile::TempDir;

use super::codex_command;

#[test]
fn list_shows_empty_state() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    let output = cmd.args(["mcp", "list"]).output()?;
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("No MCP servers configured yet."));

    Ok(())
}

#[tokio::test]
async fn list_and_get_render_expected_output() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut add = codex_command(codex_home.path())?;
    add.args([
        "mcp",
        "add",
        "docs",
        "--env",
        "TOKEN=secret",
        "--",
        "docs-server",
        "--port",
        "4000",
    ])
    .assert()
    .success();

    let mut servers = load_global_mcp_servers(codex_home.path()).await?;
    let docs_entry = servers
        .get_mut("docs")
        .expect("docs server should exist after add");
    match &mut docs_entry.transport {
        McpServerTransportConfig::Stdio { env_vars, .. } => {
            *env_vars = vec!["APP_TOKEN".into(), "WORKSPACE_ID".into()];
        }
        other => panic!("unexpected transport: {other:?}"),
    }
    ConfigEditsBuilder::new(codex_home.path())
        .replace_mcp_servers(&servers)
        .apply_blocking()?;

    let mut list_cmd = codex_command(codex_home.path())?;
    let list_output = list_cmd.args(["mcp", "list"]).output()?;
    assert!(list_output.status.success());
    let stdout = String::from_utf8(list_output.stdout)?;
    assert!(stdout.contains("Name"));
    assert!(stdout.contains("docs"));
    assert!(stdout.contains("docs-server"));
    assert!(stdout.contains("TOKEN=*****"));
    assert!(stdout.contains("APP_TOKEN=*****"));
    assert!(stdout.contains("WORKSPACE_ID=*****"));
    assert!(stdout.contains("Status"));
    assert!(stdout.contains("Auth"));
    assert!(stdout.contains("enabled"));
    assert!(stdout.contains("Unsupported"));

    let mut list_json_cmd = codex_command(codex_home.path())?;
    let json_output = list_json_cmd.args(["mcp", "list", "--json"]).output()?;
    assert!(json_output.status.success());
    let stdout = String::from_utf8(json_output.stdout)?;
    let parsed: JsonValue = serde_json::from_str(&stdout)?;
    assert_eq!(
        parsed,
        json!([
          {
            "name": "docs",
            "enabled": true,
            "disabled_reason": null,
            "environment_id": "local",
            "required": false,
            "supports_parallel_tool_calls": false,
            "default_tools_approval_mode": null,
            "transport": {
              "type": "stdio",
              "command": "docs-server",
              "args": [
                "--port",
                "4000"
              ],
              "env": {
                "TOKEN": "*****"
              },
              "env_vars": [
                "APP_TOKEN",
                "WORKSPACE_ID"
              ],
              "cwd": null
            },
            "startup_timeout_sec": null,
            "tool_timeout_sec": null,
            "auth_status": "unsupported"
          }
        ]
        )
    );

    let mut get_cmd = codex_command(codex_home.path())?;
    let get_output = get_cmd.args(["mcp", "get", "docs"]).output()?;
    assert!(get_output.status.success());
    let stdout = String::from_utf8(get_output.stdout)?;
    assert!(stdout.contains("docs"));
    assert!(stdout.contains("transport: stdio"));
    assert!(stdout.contains("command: docs-server"));
    assert!(stdout.contains("args: --port 4000"));
    assert!(stdout.contains("env: TOKEN=*****"));
    assert!(stdout.contains("APP_TOKEN=*****"));
    assert!(stdout.contains("WORKSPACE_ID=*****"));
    assert!(stdout.contains("enabled: true"));
    assert!(stdout.contains("remove: codex mcp remove docs"));

    let mut get_json_cmd = codex_command(codex_home.path())?;
    get_json_cmd
        .args(["mcp", "get", "docs", "--json"])
        .assert()
        .success()
        .stdout(contains("\"name\": \"docs\"").and(contains("\"enabled\": true")));

    for args in [
        vec!["mcp", "list", "--json"],
        vec!["mcp", "get", "docs", "--json"],
    ] {
        let hidden = codex_command(codex_home.path())?
            .args(&args)
            .assert()
            .success();
        let hidden: JsonValue = serde_json::from_slice(&hidden.get_output().stdout)?;
        let entry = if hidden.is_array() {
            &hidden[0]
        } else {
            &hidden
        };
        assert_eq!(entry["transport"]["env"]["TOKEN"], "*****");
        let revealed = codex_command(codex_home.path())?
            .args(&args)
            .arg("--show-secrets")
            .assert()
            .success();
        let revealed: JsonValue = serde_json::from_slice(&revealed.get_output().stdout)?;
        let entry = if revealed.is_array() {
            &revealed[0]
        } else {
            &revealed
        };
        assert_eq!(entry["transport"]["env"]["TOKEN"], "secret");
    }
    Ok(())
}

#[tokio::test]
async fn get_disabled_server_shows_single_line() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut add = codex_command(codex_home.path())?;
    add.args(["mcp", "add", "docs", "--", "docs-server"])
        .assert()
        .success();

    let mut servers = load_global_mcp_servers(codex_home.path()).await?;
    let docs = servers
        .get_mut("docs")
        .expect("docs server should exist after add");
    docs.enabled = false;
    ConfigEditsBuilder::new(codex_home.path())
        .replace_mcp_servers(&servers)
        .apply_blocking()?;

    let mut get_cmd = codex_command(codex_home.path())?;
    let get_output = get_cmd.args(["mcp", "get", "docs"]).output()?;
    assert!(get_output.status.success());
    let stdout = String::from_utf8(get_output.stdout)?;
    assert_eq!(stdout.trim_end(), "docs (disabled)");

    Ok(())
}

#[test]
fn selected_profile_controls_mcp_inspection_and_masks_http_headers() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"[mcp_servers.docs]
url = "http://127.0.0.1:8/global"
"#,
    )?;
    std::fs::write(
        codex_home.path().join("work.config.toml"),
        r#"[mcp_servers.docs]
url = "http://127.0.0.1:9/mcp"
required = true
supports_parallel_tool_calls = true
http_headers = { Authorization = "Bearer secret" }
"#,
    )?;
    for command in [
        vec!["mcp", "list", "--json"],
        vec!["mcp", "get", "docs", "--json"],
    ] {
        for reveal in [false, true] {
            let mut cmd = codex_command(codex_home.path())?;
            cmd.args(["--profile", "work"]).args(&command);
            if reveal {
                cmd.arg("--show-secrets");
            }
            let output = cmd.assert().success();
            let value: JsonValue = serde_json::from_slice(&output.get_output().stdout)?;
            let entry = if value.is_array() { &value[0] } else { &value };
            assert_eq!(entry["transport"]["url"], "http://127.0.0.1:9/mcp");
            assert_eq!(entry["required"], true);
            assert_eq!(entry["supports_parallel_tool_calls"], true);
            assert_eq!(
                entry["transport"]["http_headers"]["Authorization"],
                if reveal { "Bearer secret" } else { "*****" }
            );
        }
    }
    Ok(())
}
