use super::*;
use crate::McpServerOAuthConfig;
use crate::McpServerToolConfig;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

#[test]
fn config_editors_share_document_loader() -> anyhow::Result<()> {
    let plugin_editor = include_str!("plugin_edit.rs");
    let mcp_editor = include_str!("mcp_edit.rs");
    let marketplace_editor = include_str!("marketplace_edit.rs");

    for (name, source) in [
        ("plugin", plugin_editor),
        ("MCP", mcp_editor),
        ("marketplace", marketplace_editor),
    ] {
        assert!(
            !source.contains("fn read_or_create_document"),
            "{name} editor should not maintain a private document loader"
        );
        assert!(source.contains("read_or_create_config_document"));
    }

    let temp = tempfile::tempdir()?;
    let config_path = temp.path().join(CONFIG_TOML_FILE);
    let empty = crate::read_or_create_config_document(Some(&config_path))?;
    assert!(empty.as_table().is_empty());

    std::fs::write(&config_path, "not = [valid")?;
    let err = crate::read_or_create_config_document(Some(&config_path))
        .expect_err("invalid TOML should be rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidData);
    Ok(())
}

#[tokio::test]
async fn load_global_mcp_servers_rejects_inline_bearer_token_via_config_parser()
-> anyhow::Result<()> {
    let unique_suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let codex_home = std::env::temp_dir().join(format!(
        "codex-config-mcp-edit-test-{}-{unique_suffix}",
        std::process::id()
    ));
    std::fs::create_dir_all(&codex_home)?;
    std::fs::write(
        codex_home.join(CONFIG_TOML_FILE),
        r#"
[mcp_servers.docs]
url = "https://example.com/mcp"
bearer_token = "secret"
"#,
    )?;

    let err = load_global_mcp_servers(&codex_home)
        .await
        .expect_err("inline bearer token should be rejected");
    assert_eq!(err.kind(), ErrorKind::InvalidData);
    assert!(err.to_string().contains("bearer_token is not supported"));

    std::fs::remove_dir_all(&codex_home)?;
    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_per_tool_approval_overrides() -> anyhow::Result<()> {
    let unique_suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let codex_home = std::env::temp_dir().join(format!(
        "codex-config-mcp-edit-test-{}-{unique_suffix}",
        std::process::id()
    ));
    let servers = BTreeMap::from([(
        "docs".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::Stdio {
                command: "docs-server".to_string(),
                args: Vec::new(),
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            environment_id: crate::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: true,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: Some(AppToolApproval::Auto),
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: None,
            oauth_resource: None,
            tools: HashMap::from([
                (
                    "search".to_string(),
                    McpServerToolConfig {
                        approval_mode: Some(AppToolApproval::Approve),
                    },
                ),
                (
                    "read".to_string(),
                    McpServerToolConfig {
                        approval_mode: Some(AppToolApproval::Prompt),
                    },
                ),
            ]),
        },
    )]);

    ConfigEditsBuilder::new(&codex_home)
        .replace_mcp_servers(&servers)
        .apply()
        .await?;

    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert_eq!(
        serialized,
        r#"[mcp_servers.docs]
command = "docs-server"
supports_parallel_tool_calls = true
default_tools_approval_mode = "auto"

[mcp_servers.docs.tools]

[mcp_servers.docs.tools.read]
approval_mode = "prompt"

[mcp_servers.docs.tools.search]
approval_mode = "approve"
"#
    );

    let loaded = load_global_mcp_servers(&codex_home).await?;
    assert_eq!(loaded, servers);

    std::fs::remove_dir_all(&codex_home)?;

    Ok(())
}

#[tokio::test]
async fn replace_mcp_servers_serializes_oauth_client_id() -> anyhow::Result<()> {
    let unique_suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let codex_home = std::env::temp_dir().join(format!(
        "codex-config-mcp-oauth-edit-test-{}-{unique_suffix}",
        std::process::id()
    ));
    let servers = BTreeMap::from([(
        "maas_outlook".to_string(),
        McpServerConfig {
            auth: Default::default(),
            transport: McpServerTransportConfig::StreamableHttp {
                url: "https://example.com/mcp".to_string(),
                bearer_token_env_var: None,
                http_headers: None,
                env_http_headers: None,
            },
            environment_id: crate::DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth: Some(McpServerOAuthConfig {
                client_id: Some("eci-prd-pub-codex-123".to_string()),
            }),
            oauth_resource: None,
            tools: HashMap::new(),
        },
    )]);

    ConfigEditsBuilder::new(&codex_home)
        .replace_mcp_servers(&servers)
        .apply()
        .await?;

    let config_path = codex_home.join(CONFIG_TOML_FILE);
    let serialized = std::fs::read_to_string(&config_path)?;
    assert_eq!(
        serialized,
        r#"[mcp_servers.maas_outlook]
url = "https://example.com/mcp"

[mcp_servers.maas_outlook.oauth]
client_id = "eci-prd-pub-codex-123"
"#
    );

    let loaded = load_global_mcp_servers(&codex_home).await?;
    assert_eq!(loaded, servers);

    std::fs::remove_dir_all(&codex_home)?;

    Ok(())
}
