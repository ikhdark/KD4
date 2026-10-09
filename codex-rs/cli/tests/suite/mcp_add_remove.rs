use anyhow::Result;
use codex_config::types::McpServerTransportConfig;
use codex_core::config::load_global_mcp_servers;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::codex_command;

#[tokio::test]
async fn add_and_remove_server_updates_global_config() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut add_cmd = codex_command(codex_home.path())?;
    add_cmd
        .args(["mcp", "add", "docs", "--", "echo", "hello"])
        .assert()
        .success()
        .stdout(contains("Added global MCP server 'docs'."));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert_eq!(servers.len(), 1);
    let docs = servers.get("docs").expect("server should exist");
    match &docs.transport {
        McpServerTransportConfig::Stdio {
            command,
            args,
            env,
            env_vars,
            cwd,
        } => {
            assert_eq!(command, "echo");
            assert_eq!(args, &vec!["hello".to_string()]);
            assert!(env.is_none());
            assert!(env_vars.is_empty());
            assert!(cwd.is_none());
        }
        other => panic!("unexpected transport: {other:?}"),
    }
    assert!(docs.enabled);

    let mut remove_cmd = codex_command(codex_home.path())?;
    remove_cmd
        .args(["mcp", "remove", "docs"])
        .assert()
        .success()
        .stdout(contains("Removed global MCP server 'docs'."));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    let mut remove_again_cmd = codex_command(codex_home.path())?;
    remove_again_cmd
        .args(["mcp", "remove", "docs"])
        .assert()
        .success()
        .stdout(contains("No MCP server named 'docs' found."));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    Ok(())
}

#[tokio::test]
async fn profile_mcp_reports_legacy_profile_migration() -> Result<()> {
    let codex_home = TempDir::new()?;
    std::fs::write(
        codex_home.path().join("config.toml"),
        r#"[profiles.work]
model = "gpt-5"
"#,
    )?;

    let mut list_cmd = codex_command(codex_home.path())?;
    list_cmd
        .args(["--profile", "work", "mcp", "list"])
        .assert()
        .failure()
        .stderr(contains("--profile `work` cannot be used"))
        .stderr(contains("[profiles.work]"))
        .stderr(contains("work.config.toml"));

    Ok(())
}

#[tokio::test]
async fn add_with_env_preserves_values() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut add_cmd = codex_command(codex_home.path())?;
    add_cmd
        .args([
            "mcp",
            "add",
            "envy",
            "--env",
            "FOO=bar=baz",
            "--env",
            "ALPHA=beta",
            "--",
            "python",
            "server.py",
        ])
        .assert()
        .success();

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    let envy = servers.get("envy").expect("server should exist");
    let env = match &envy.transport {
        McpServerTransportConfig::Stdio { env: Some(env), .. } => env,
        other => panic!("unexpected transport: {other:?}"),
    };

    assert_eq!(env.len(), 2);
    assert_eq!(env.get("FOO"), Some(&"bar=baz".to_string()));
    assert_eq!(env.get("ALPHA"), Some(&"beta".to_string()));
    assert!(envy.enabled);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn add_streamable_http_persists_transport_and_oauth_options() -> Result<()> {
    // The CLI probes OAuth support. A local 404 keeps this configuration test
    // independent of public networking; another worker serves the blocking CLI.
    let server = wiremock::MockServer::start().await;
    let url = format!("{}/mcp", server.uri());
    for (extra, token, client_id, resource) in [
        (vec![], None, None, None),
        (vec!["--bearer-token-env-var", "GITHUB_TOKEN"], Some("GITHUB_TOKEN"), None, None),
        (
            vec!["--oauth-client-id", "eci-prd-pub-codex-123", "--oauth-resource", "https://resource.example.com"],
            None,
            Some("eci-prd-pub-codex-123"),
            Some("https://resource.example.com"),
        ),
    ] {
        let codex_home = TempDir::new()?;
        codex_command(codex_home.path())?
            .env("NO_PROXY", "127.0.0.1,localhost")
            .env("no_proxy", "127.0.0.1,localhost")
            .env_remove("GITHUB_TOKEN")
            .args(["mcp", "add", "server", "--url", &url])
            .args(extra)
            .timeout(std::time::Duration::from_secs(15))
            .assert()
            .success();
        let servers = load_global_mcp_servers(codex_home.path()).await?;
        assert_eq!(servers.len(), 1);
        let configured = servers.get("server").expect("server should exist");
        match &configured.transport {
            McpServerTransportConfig::StreamableHttp {
                url: actual_url,
                bearer_token_env_var,
                http_headers,
                env_http_headers,
            } => {
                assert_eq!(actual_url, &url);
                assert_eq!(bearer_token_env_var.as_deref(), token);
                assert!(http_headers.is_none());
                assert!(env_http_headers.is_none());
            }
            other => panic!("unexpected transport: {other:?}"),
        }
        assert!(configured.enabled);
        assert_eq!(configured.oauth_client_id(), client_id);
        assert_eq!(configured.oauth_resource.as_deref(), resource);
        assert!(!codex_home.path().join(".credentials.json").exists());
        assert!(!codex_home.path().join(".env").exists());
    }
    Ok(())
}
#[tokio::test]
async fn add_streamable_http_rejects_removed_flag() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut add_cmd = codex_command(codex_home.path())?;
    add_cmd
        .args([
            "mcp",
            "add",
            "github",
            "--url",
            "https://example.com/mcp",
            "--with-bearer-token",
        ])
        .assert()
        .failure()
        .stderr(contains("--with-bearer-token"));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    Ok(())
}

#[tokio::test]
async fn add_cant_add_command_and_url() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut add_cmd = codex_command(codex_home.path())?;
    add_cmd
        .args([
            "mcp",
            "add",
            "github",
            "--url",
            "https://example.com/mcp",
            "--",
            "echo",
            "hello",
        ])
        .assert()
        .failure()
        .stderr(contains("cannot be used with"))
        .stderr(contains("--url"));

    let servers = load_global_mcp_servers(codex_home.path()).await?;
    assert!(servers.is_empty());

    Ok(())
}
