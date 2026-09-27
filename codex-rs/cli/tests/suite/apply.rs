use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::Result;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::codex_command;

fn init_repo(repo: &Path) -> Result<()> {
    std::fs::create_dir_all(repo)?;
    for args in [
        vec!["init"],
        vec![
            "-c",
            "user.name=Test User",
            "-c",
            "user.email=test@example.com",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "Initial commit",
        ],
    ] {
        let output = Command::new("git")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .current_dir(repo)
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "git setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apply_uses_selected_directory_for_config_and_patch() -> Result<()> {
    let home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let launch = workspace.path().join("launch");
    let target = workspace.path().join("target");
    init_repo(&launch)?;
    init_repo(&target)?;
    let server = MockServer::start().await;

    // Reject invalid settings from the selected repository before fetching or
    // applying a patch. Project config cannot choose the credential endpoint.
    let mut config = toml::Table::new();
    config.insert("cli_auth_credentials_store".into(), "file".into());
    config.insert("chatgpt_base_url".into(), server.uri().into());
    let mut projects = toml::Table::new();
    let mut trust = toml::Table::new();
    trust.insert("trust_level".into(), "trusted".into());
    projects.insert(target.to_string_lossy().into_owned(), trust.into());
    config.insert("projects".into(), projects.into());
    std::fs::write(home.path().join("config.toml"), toml::to_string(&config)?)?;
    std::fs::create_dir(target.join(".codex"))?;
    std::fs::write(
        target.join(".codex/config.toml"),
        "model_reasoning_effort = [\n",
    )?;
    std::fs::write(
        home.path().join("auth.json"),
        serde_json::to_vec(&json!({
            "auth_mode": "chatgpt",
            "last_refresh": "2099-01-01T00:00:00Z",
            "tokens": {
                "id_token": "eyJhbGciOiJub25lIn0.e30.c2ln",
                "access_token": "test-access",
                "refresh_token": "test-refresh",
                "account_id": "apply-test-account"
            }
        }))?,
    )?;
    Mock::given(method("GET"))
        .and(path("/wham/tasks/task-apply"))
        .and(header("chatgpt-account-id", "apply-test-account"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "current_diff_task_turn": {"output_items": [{
                "type": "output_diff",
                "diff": "diff --git a/applied.txt b/applied.txt\nnew file mode 100644\n--- /dev/null\n+++ b/applied.txt\n@@ -0,0 +1 @@\n+cloud change\n"
            }]}
        })))
        .expect(1)
        .mount(&server)
        .await;

    let mut command = codex_command(home.path())?;
    command
        .current_dir(&launch)
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("CODEX_ACCESS_TOKEN")
        .env_remove("CODEX_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .args(["-C", "../target", "apply", "task-apply"])
        .timeout(Duration::from_secs(30));
    command
        .assert()
        .failure()
        .stderr(contains("Error parsing project config file"));
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!target.join("applied.txt").exists());
    assert!(!launch.join("applied.txt").exists());

    std::fs::write(
        target.join(".codex/config.toml"),
        "model_reasoning_effort = 'high'\n",
    )?;
    command
        .assert()
        .success()
        .stdout(contains("Successfully applied diff"));

    assert_eq!(
        std::fs::read_to_string(target.join("applied.txt"))?,
        "cloud change\n"
    );
    assert!(!launch.join("applied.txt").exists());
    server.verify().await;
    Ok(())
}
