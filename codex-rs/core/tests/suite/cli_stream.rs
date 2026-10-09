use codex_login::CODEX_ACCESS_TOKEN_ENV_VAR;
use codex_login::CODEX_API_KEY_ENV_VAR;
use codex_protocol::openai_models::ModelsResponse;
use core_test_support::fs_wait;
use core_test_support::require_network;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use std::io;

use std::process::Command;
use std::process::Output;
use std::time::Duration;
use tempfile::TempDir;
use uuid::Uuid;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const PERSONAL_ACCESS_TOKEN: &str = "at-cli-test";
const PERSONAL_ACCESS_TOKEN_AUTHORIZATION: &str = "Bearer at-cli-test";
const PERSONAL_ACCESS_TOKEN_ACCOUNT_ID: &str = "account-pat";
const WHOAMI_PATH: &str = "/v1/user-auth-credential/whoami";
const CLOUD_CONFIG_BUNDLE_PATH: &str = "/backend-api/wham/config/bundle";
const CLI_TIMEOUT: Duration = Duration::from_secs(30);

fn repo_root() -> std::path::PathBuf {
    codex_utils_cargo_bin::repo_root().expect("failed to resolve repo root")
}

fn cli_sse_response() -> String {
    responses::sse(vec![
        responses::ev_response_created("resp-fixture"),
        responses::ev_assistant_message("msg-fixture", "fixture hello"),
        responses::ev_completed("resp-fixture"),
    ])
}

fn sse_provider_override(server: &MockServer, base_path: &str) -> String {
    format!(
        "model_providers.mock={{ name = \"mock\", base_url = \"{}{base_path}\", env_key = \"PATH\", wire_api = \"responses\", supports_websockets = false }}",
        server.uri()
    )
}

async fn mount_personal_access_token_startup(server: &MockServer) {
    let _models = responses::mount_models_once(server, ModelsResponse { models: Vec::new() }).await;
    Mock::given(method("GET"))
        .and(path(WHOAMI_PATH))
        .and(header("authorization", PERSONAL_ACCESS_TOKEN_AUTHORIZATION))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "email": "user@example.com",
            "chatgpt_user_id": "user-pat",
            "chatgpt_account_id": PERSONAL_ACCESS_TOKEN_ACCOUNT_ID,
            "chatgpt_plan_type": "enterprise",
            "chatgpt_account_is_fedramp": true,
        })))
        .expect(1..)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(CLOUD_CONFIG_BUNDLE_PATH))
        .and(header("authorization", PERSONAL_ACCESS_TOKEN_AUTHORIZATION))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(server)
        .await;
}

#[expect(clippy::unwrap_used)]
fn personal_access_token_exec_command(server: &MockServer, home: &TempDir) -> Command {
    let bin = codex_utils_cargo_bin::cargo_bin("codex").unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(format!(
            "model_providers.pat={{ name = \"pat\", base_url = \"{}/api/codex\", wire_api = \"responses\", requires_openai_auth = true, supports_websockets = false, request_max_retries = 0, stream_max_retries = 0 }}",
            server.uri()
        ))
        .arg("-c")
        .arg("model_provider=\"pat\"")
        .arg("-c")
        .arg(format!("chatgpt_base_url=\"{}/backend-api\"", server.uri()))
        .arg("-C")
        .arg(home.path())
        .arg("hello?");
    cmd.env("CODEX_HOME", home.path())
        .env_remove(codex_state::SQLITE_HOME_ENV)
        .env(CODEX_ACCESS_TOKEN_ENV_VAR, PERSONAL_ACCESS_TOKEN)
        .env("CODEX_AUTHAPI_BASE_URL", server.uri())
        .env_remove(CODEX_API_KEY_ENV_VAR)
        .env_remove("OPENAI_API_KEY");
    cmd
}

// Own the process tree and both output streams under one deadline.
async fn run_cli_command(command: Command) -> io::Result<Output> {
    core_test_support::process::capture_contained_command(
        &mut tokio::process::Command::from(command),
        CLI_TIMEOUT,
        /*mirror_output*/ false,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_mode_stream_cli_supports_personal_access_tokens() {
    require_network!();

    let server = MockServer::start().await;
    mount_personal_access_token_startup(&server).await;
    let resp_mock = responses::mount_sse_once(&server, cli_sse_response()).await;
    let home = TempDir::new().unwrap();

    let cmd = personal_access_token_exec_command(&server, &home);
    let output = run_cli_command(cmd).await.expect("CLI completes");

    assert!(
        output.status.success(),
        "codex-cli exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = resp_mock.single_request();
    assert_eq!(request.path(), "/api/codex/responses");
    assert_eq!(
        request.header("authorization").as_deref(),
        Some("Bearer at-cli-test")
    );
    assert_eq!(
        request.header("chatgpt-account-id").as_deref(),
        Some(PERSONAL_ACCESS_TOKEN_ACCOUNT_ID)
    );
    assert_eq!(request.header("x-openai-fedramp").as_deref(), Some("true"));
    server.verify().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_mode_stream_cli_does_not_attempt_oauth_refresh_for_personal_access_tokens_after_401()
 {
    require_network!();

    let server = MockServer::start().await;
    mount_personal_access_token_startup(&server).await;
    Mock::given(method("POST"))
        .and(path("/api/codex/responses"))
        .and(header("authorization", PERSONAL_ACCESS_TOKEN_AUTHORIZATION))
        .and(header(
            "chatgpt-account-id",
            PERSONAL_ACCESS_TOKEN_ACCOUNT_ID,
        ))
        .and(header("x-openai-fedramp", "true"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .expect(1..)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let home = TempDir::new().unwrap();

    let cmd = personal_access_token_exec_command(&server, &home);
    let output = run_cli_command(cmd).await.expect("CLI completes");

    assert!(!output.status.success());
    server.verify().await;
}

/// Tests streaming the Responses API through the CLI using a mock server.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_mode_stream_cli() {
    require_network!();

    let server = MockServer::start().await;
    let _models_mock =
        responses::mount_models_once(&server, ModelsResponse { models: Vec::new() }).await;
    let repo_root = repo_root();
    let sse = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "hi"),
        responses::ev_completed("resp-1"),
    ]);
    let resp_mock = responses::mount_sse_once(&server, sse).await;

    let home = TempDir::new().unwrap();
    let provider_override = sse_provider_override(&server, "/v1");
    let bin = codex_utils_cargo_bin::cargo_bin("codex").unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(&provider_override)
        .arg("-c")
        .arg("model_provider=\"mock\"")
        .arg("-C")
        .arg(&repo_root)
        .arg("hello?");
    cmd.env("CODEX_HOME", home.path())
        .env_remove(codex_state::SQLITE_HOME_ENV)
        .env("OPENAI_API_KEY", "dummy");

    let output = run_cli_command(cmd).await.unwrap();
    println!("Status: {}", output.status);
    println!("Stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    println!("Stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let hi_lines = stdout.lines().filter(|line| line.trim() == "hi").count();
    assert_eq!(hi_lines, 1, "Expected exactly one line with 'hi'");

    let request = resp_mock.single_request();
    assert_eq!(request.path(), "/v1/responses");
}

/// Ensures `openai_base_url` config override routes built-in openai provider requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_mode_stream_cli_supports_openai_base_url_config_override() {
    require_network!();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(426))
        .mount(&server)
        .await;
    let repo_root = repo_root();
    let sse = responses::sse(vec![
        responses::ev_response_created("resp-1"),
        responses::ev_assistant_message("msg-1", "hi"),
        responses::ev_completed("resp-1"),
    ]);
    let resp_mock = responses::mount_sse_once(&server, sse).await;

    let home = TempDir::new().unwrap();
    let bin = codex_utils_cargo_bin::cargo_bin("codex").unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(format!("openai_base_url=\"{}/v1\"", server.uri()))
        .arg("-C")
        .arg(&repo_root)
        .arg("hello?");
    cmd.env("CODEX_HOME", home.path())
        .env_remove(codex_state::SQLITE_HOME_ENV)
        .env("OPENAI_API_KEY", "dummy");

    let output = run_cli_command(cmd).await.unwrap();
    assert!(
        output.status.success(),
        "codex-cli exec failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let request = resp_mock.single_request();
    assert_eq!(request.path(), "/v1/responses");
}

/// Verify that passing `-c model_instructions_file=...` to the CLI
/// overrides the built-in base instructions by inspecting the request body
/// received by a mock OpenAI Responses endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_cli_applies_model_instructions_file() {
    require_network!();

    // Start mock server which will capture the request and return a minimal
    // SSE stream for a single turn.
    let server = MockServer::start().await;
    let sse = concat!(
        "data: {\"type\":\"response.created\",\"response\":{}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n"
    );
    let resp_mock = core_test_support::responses::mount_sse_once(&server, sse.to_string()).await;

    // Create a temporary instructions file with a unique marker we can assert
    // appears in the outbound request payload.
    let custom = TempDir::new().unwrap();
    let marker = "cli-model-instructions-file-marker";
    let custom_path = custom.path().join("instr.md");
    std::fs::write(&custom_path, marker).unwrap();
    let custom_path_str = custom_path.to_string_lossy().replace('\\', "/");

    // Build a provider override that points at the mock server and instructs
    // Codex to use the Responses API with the dummy env var.
    let provider_override = format!(
        "model_providers.mock={{ name = \"mock\", base_url = \"{}/v1\", env_key = \"PATH\", wire_api = \"responses\" }}",
        server.uri()
    );

    let home = TempDir::new().unwrap();
    let repo_root = repo_root();
    let bin = codex_utils_cargo_bin::cargo_bin("codex").unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--model")
        .arg("gpt-5.5")
        .arg("-c")
        .arg(&provider_override)
        .arg("-c")
        .arg("model_provider=\"mock\"")
        .arg("-c")
        .arg(format!("model_instructions_file=\"{custom_path_str}\""))
        .arg("-C")
        .arg(&repo_root)
        .arg("hello?\n");
    cmd.env("CODEX_HOME", home.path())
        .env_remove(codex_state::SQLITE_HOME_ENV)
        .env("OPENAI_API_KEY", "dummy");

    let output = run_cli_command(cmd).await.unwrap();
    println!("Status: {}", output.status);
    println!("Stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    println!("Stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success());

    // Inspect the captured request and verify our custom base instructions were
    // included in the `instructions` field.
    let request = resp_mock.single_request();
    let body = request.body_json();
    let instructions = body
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        instructions.contains(marker),
        "instructions did not contain custom marker; got: {instructions}"
    );
}

/// Verify that `codex exec --profile ...` preserves the active user config
/// profile when it starts the in-process app-server thread, so the selected
/// profile's `model_instructions_file` reaches the outbound request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_cli_profile_applies_model_instructions_file() {
    require_network!();

    let server = MockServer::start().await;
    let sse = concat!(
        "data: {\"type\":\"response.created\",\"response\":{}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\"}}\n\n"
    );
    let resp_mock = core_test_support::responses::mount_sse_once(&server, sse.to_string()).await;

    let custom = TempDir::new().unwrap();
    let marker = "cli-profile-model-instructions-file-marker";
    let custom_path = custom.path().join("instr.md");
    std::fs::write(&custom_path, marker).unwrap();
    let custom_path_str = custom_path.to_string_lossy().replace('\\', "/");

    let provider_override = format!(
        "model_providers.mock={{ name = \"mock\", base_url = \"{}/v1\", env_key = \"PATH\", wire_api = \"responses\" }}",
        server.uri()
    );

    let home = TempDir::new().unwrap();
    std::fs::write(
        home.path().join("default.config.toml"),
        format!("model_instructions_file = \"{custom_path_str}\"\n"),
    )
    .unwrap();

    let repo_root = repo_root();
    let bin = codex_utils_cargo_bin::cargo_bin("codex").unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--model")
        .arg("gpt-5.5")
        .arg("--profile")
        .arg("default")
        .arg("-c")
        .arg(&provider_override)
        .arg("-c")
        .arg("model_provider=\"mock\"")
        .arg("-C")
        .arg(&repo_root)
        .arg("hello?\n");
    cmd.env("CODEX_HOME", home.path())
        .env_remove(codex_state::SQLITE_HOME_ENV)
        .env("OPENAI_API_KEY", "dummy");

    let output = run_cli_command(cmd).await.unwrap();
    println!("Status: {}", output.status);
    println!("Stdout:\n{}", String::from_utf8_lossy(&output.stdout));
    println!("Stderr:\n{}", String::from_utf8_lossy(&output.stderr));
    assert!(output.status.success());

    let request = resp_mock.single_request();
    let body = request.body_json();
    let instructions = body
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    assert!(
        instructions.contains(marker),
        "instructions did not contain profile marker; got: {instructions}"
    );
}



/// End-to-end: create a session (writes rollout), verify the file, then resume and confirm append.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integration_creates_and_checks_session_file() -> anyhow::Result<()> {
    // Honor sandbox network restrictions for CI parity with the other tests.
    require_network!();

    // 1. Temp home so we read/write isolated session files.
    let home = TempDir::new()?;

    // 2. Unique marker we'll look for in the session log.
    let marker = format!("integration-test-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    // 3. Serve two hermetic SSE responses, one for the initial run and one for resume.
    let server = MockServer::start().await;
    let resp_mock =
        responses::mount_sse_sequence(&server, vec![cli_sse_response(), cli_sse_response()]).await;
    let repo_root = repo_root();

    // 4. Run the codex CLI and invoke `exec`, which is what records a session.
    let bin = codex_utils_cargo_bin::cargo_bin("codex").unwrap();
    let mut cmd = Command::new(bin);
    cmd.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(sse_provider_override(&server, "/v1"))
        .arg("-c")
        .arg("model_provider=\"mock\"")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt);
    cmd.env("CODEX_HOME", home.path())
        .env_remove(codex_state::SQLITE_HOME_ENV)
        .env(CODEX_API_KEY_ENV_VAR, "dummy");

    let output = run_cli_command(cmd).await.unwrap();
    assert!(
        output.status.success(),
        "codex-cli exec failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Wait for sessions dir to appear.
    let sessions_dir = home.path().join("sessions");
    fs_wait::wait_for_path_exists(&sessions_dir, Duration::from_secs(5)).await?;

    // Find the session file that contains `marker`.
    let marker_clone = marker.clone();
    let path = fs_wait::wait_for_matching_file(&sessions_dir, Duration::from_secs(10), move |p| {
        if p.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            return false;
        }
        let Ok(content) = std::fs::read_to_string(p) else {
            return false;
        };
        content.contains(&marker_clone)
    })
    .await?;

    // Basic sanity checks on location and metadata.
    let rel = path
        .strip_prefix(&sessions_dir)
        .expect("session file should live under sessions/");
    let comps: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        comps.len(),
        4,
        "Expected sessions/YYYY/MM/DD/<file>, got {rel:?}"
    );
    let year = &comps[0];
    let month = &comps[1];
    let day = &comps[2];
    assert!(
        year.len() == 4 && year.chars().all(|c| c.is_ascii_digit()),
        "Year dir not 4-digit numeric: {year}"
    );
    assert!(
        month.len() == 2 && month.chars().all(|c| c.is_ascii_digit()),
        "Month dir not zero-padded 2-digit numeric: {month}"
    );
    assert!(
        day.len() == 2 && day.chars().all(|c| c.is_ascii_digit()),
        "Day dir not zero-padded 2-digit numeric: {day}"
    );
    if let Ok(m) = month.parse::<u8>() {
        assert!((1..=12).contains(&m), "Month out of range: {m}");
    }
    if let Ok(d) = day.parse::<u8>() {
        assert!((1..=31).contains(&d), "Day out of range: {d}");
    }

    let content = std::fs::read_to_string(&path).expect("failed to read session file");
    let lines = content.lines().skip(1);
    let mut reader = codex_rollout::open_rollout_line_reader(&path).await?;
    let meta_line = reader
        .next_line()
        .await?
        .expect("missing session meta line");
    let meta: serde_json::Value =
        serde_json::from_str(&meta_line).expect("failed to parse session meta line as JSON");
    assert_eq!(
        meta.get("type").and_then(|v| v.as_str()),
        Some("session_meta")
    );
    let payload = meta.get("payload").expect("Missing payload in meta line");
    assert!(payload.get("id").is_some(), "SessionMeta missing id");
    assert!(
        payload.get("timestamp").is_some(),
        "SessionMeta missing timestamp"
    );

    let mut found_message = false;
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(item) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if item.get("type").and_then(|t| t.as_str()) == Some("response_item")
            && let Some(payload) = item.get("payload")
            && payload.get("type").and_then(|t| t.as_str()) == Some("message")
            && let Some(c) = payload.get("content")
            && c.to_string().contains(&marker)
        {
            found_message = true;
            break;
        }
    }
    assert!(
        found_message,
        "No message found in session file containing the marker"
    );

    // Second run: resume should update the existing file.
    let marker2 = format!("integration-resume-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");
    let bin2 = codex_utils_cargo_bin::cargo_bin("codex").unwrap();
    let mut cmd2 = Command::new(bin2);
    cmd2.arg("exec")
        .arg("--skip-git-repo-check")
        .arg("-c")
        .arg(sse_provider_override(&server, "/v1"))
        .arg("-c")
        .arg("model_provider=\"mock\"")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt2)
        .arg("resume")
        .arg("--last");
    cmd2.env("CODEX_HOME", home.path())
        .env_remove(codex_state::SQLITE_HOME_ENV)
        .env("OPENAI_API_KEY", "dummy");

    let output2 = run_cli_command(cmd2).await.unwrap();
    assert!(
        output2.status.success(),
        "resume codex-cli run failed with status {}\nstdout:\n{}\nstderr:\n{}",
        output2.status,
        String::from_utf8_lossy(&output2.stdout),
        String::from_utf8_lossy(&output2.stderr),
    );
    assert_eq!(resp_mock.requests().len(), 2);

    // Find the new session file containing the resumed marker.
    let marker2_clone = marker2.clone();
    let resumed_path =
        fs_wait::wait_for_matching_file(&sessions_dir, Duration::from_secs(10), move |p| {
            if p.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                return false;
            }
            std::fs::read_to_string(p)
                .map(|content| content.contains(&marker2_clone))
                .unwrap_or(false)
        })
        .await?;

    // Resume should write to the existing log file.
    assert_eq!(
        resumed_path, path,
        "resume should append to the existing session file"
    );

    let resumed_content = std::fs::read_to_string(&resumed_path)?;
    assert!(
        resumed_content.contains(&marker),
        "resumed file missing original marker"
    );
    assert!(
        resumed_content.contains(&marker2),
        "resumed file missing resumed marker"
    );
    Ok(())
}
