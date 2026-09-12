#![allow(clippy::unwrap_used)]
use anyhow::Context;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex_exec::test_codex_exec;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::string::ToString;
use tempfile::TempDir;
use uuid::Uuid;
use walkdir::WalkDir;
use wiremock::MockServer;

/// Utility: scan the sessions dir for a rollout file that contains `marker`
/// in any response_item.message.content entry. Returns the absolute path.
fn find_session_file_containing_marker(
    sessions_dir: &std::path::Path,
    marker: &str,
) -> Option<std::path::PathBuf> {
    for entry in WalkDir::new(sessions_dir) {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if !entry.file_name().to_string_lossy().ends_with(".jsonl") {
            continue;
        }
        let path = entry.path();
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        // Skip the first meta line and scan remaining JSONL entries.
        let mut lines = content.lines();
        if lines.next().is_none() {
            continue;
        }
        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(item): Result<Value, _> = serde_json::from_str(line) else {
                continue;
            };
            if item.get("type").and_then(|t| t.as_str()) == Some("response_item")
                && let Some(payload) = item.get("payload")
                && payload.get("type").and_then(|t| t.as_str()) == Some("message")
                && payload
                    .get("content")
                    .map(ToString::to_string)
                    .unwrap_or_default()
                    .contains(marker)
            {
                return Some(path.to_path_buf());
            }
        }
    }
    None
}

/// Extract the conversation UUID from the first SessionMeta line in the rollout file.
fn extract_conversation_id(path: &std::path::Path) -> String {
    let content = std::fs::read_to_string(path).unwrap();
    let mut lines = content.lines();
    let meta_line = lines.next().expect("missing meta line");
    let meta: Value = serde_json::from_str(meta_line).expect("invalid meta json");
    meta.get("payload")
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

fn max_user_image_count(path: &std::path::Path) -> usize {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let mut max_count = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(item): Result<Value, _> = serde_json::from_str(line) else {
            continue;
        };
        if item.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue;
        }
        let Some(payload) = item.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        if payload.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let Some(content_items) = payload.get("content").and_then(|v| v.as_array()) else {
            continue;
        };
        let count = content_items
            .iter()
            .filter(|entry| entry.get("type").and_then(|t| t.as_str()) == Some("input_image"))
            .count();
        max_count = max_count.max(count);
    }
    max_count
}

fn exec_repo_root() -> anyhow::Result<std::path::PathBuf> {
    Ok(codex_utils_cargo_bin::repo_root()?)
}

fn exec_sse_response(index: usize) -> String {
    let response_id = format!("resp-exec-{index}");
    let message_id = format!("msg-exec-{index}");
    responses::sse(vec![
        responses::ev_response_created(&response_id),
        responses::ev_assistant_message(&message_id, "exec response"),
        responses::ev_completed(&response_id),
    ])
}

async fn mount_exec_responses(
    server: &MockServer,
    count: usize,
) -> core_test_support::responses::ResponseMock {
    responses::mount_sse_sequence(server, (0..count).map(exec_sse_response).collect()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_fails_when_history_is_empty() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("resume")
        .arg("--last")
        .arg("echo should not start a new thread")
        .assert()
        .failure()
        .stderr(contains("no resumable session found"));

    Ok(())
}

#[test]
fn exec_resume_without_selector_rejects_before_state_and_environment_initialization() {
    let test = test_codex_exec();
    std::fs::write(test.home_path().join("environments.toml"), "invalid = [")
        .expect("write malformed environment config");
    let state_db_path = test.home_path().join("state_5.sqlite");

    test.cmd()
        .arg("--skip-git-repo-check")
        .arg("resume")
        .assert()
        .failure()
        .stderr(contains(
            "resume requires a session ID or name, or the --last flag",
        ));

    assert!(
        !state_db_path.exists(),
        "deterministic resume rejection must not create the state database"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_reports_when_provider_filter_excludes_history() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 1).await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("create a session for the default provider")
        .assert()
        .success();

    let alternate_provider = format!(
        "model_providers.alternate={{ name = 'Alternate', base_url = '{}/v1', wire_api = 'responses' }}",
        server.uri()
    );
    test.cmd_with_server(&server)
        .arg("-c")
        .arg(alternate_provider)
        .arg("-c")
        .arg("model_provider='alternate'")
        .arg("--skip-git-repo-check")
        .arg("resume")
        .arg("--last")
        .arg("do not start a new thread")
        .assert()
        .failure()
        .stderr(contains(
            "resumable sessions were found, but none match model provider `alternate`",
        ));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_reports_when_cwd_filter_excludes_history() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 1).await;
    let first_cwd = TempDir::new()?;
    let other_cwd = TempDir::new()?;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(first_cwd.path())
        .arg("create a session in the first directory")
        .assert()
        .success();

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(other_cwd.path())
        .arg("resume")
        .arg("--last")
        .arg("do not start a new thread")
        .assert()
        .failure()
        .stderr(contains("none match the current working directory"))
        .stderr(contains("use --all"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_unknown_name_fails_instead_of_starting_thread() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("resume")
        .arg("missing-thread-name")
        .arg("echo should not start a new thread")
        .assert()
        .failure()
        .stderr(contains(
            "no resumable session named `missing-thread-name` found",
        ));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_appends_to_existing_file() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-exec-0"),
                responses::ev_assistant_message("msg-exec-0", "exec response"),
                responses::ev_completed_with_tokens("resp-exec-0", /*total_tokens*/ 7),
            ]),
            exec_sse_response(/*index*/ 1),
        ],
    )
    .await;
    let repo_root = exec_repo_root()?;

    // 1) First run: create a session with a unique marker in the content.
    let marker = format!("resume-last-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    // Find the created session file containing the marker.
    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");

    // 2) Second run: resume the most recent file with a new marker.
    let marker2 = format!("resume-last-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");

    let output = test
        .cmd_with_server(&server)
        .env("RUST_LOG", "codex_app_server::outgoing_message=trace")
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt2)
        .arg("resume")
        .arg("--last")
        .output()
        .context("resume run should succeed")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "resume failed: {stderr}");
    assert_eq!(
        stderr
            .matches("app-server event: thread/tokenUsage/updated")
            .count(),
        1,
        "resume should not replay restored token usage: {stderr}"
    );

    // Ensure the same file was updated and contains both markers.
    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no resumed session file containing marker2");
    assert_eq!(
        resumed_path, path,
        "resume --last should append to existing file"
    );
    let content = std::fs::read_to_string(&resumed_path)?;
    assert!(content.contains(&marker));
    assert!(content.contains(&marker2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_accepts_prompt_after_flag_in_json_mode() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let repo_root = exec_repo_root()?;

    // 1) First run: create a session with a unique marker in the content.
    let marker = format!("resume-last-json-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    // Find the created session file containing the marker.
    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");

    // 2) Second run: resume the most recent file and pass the prompt after --last.
    let marker2 = format!("resume-last-json-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg("--json")
        .arg("resume")
        .arg("--last")
        .arg(&prompt2)
        .assert()
        .success();

    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no resumed session file containing marker2");
    assert_eq!(
        resumed_path, path,
        "resume --last should append to existing file"
    );
    let content = std::fs::read_to_string(&resumed_path)?;
    assert!(content.contains(&marker));
    assert!(content.contains(&marker2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_respects_cwd_filter_and_all_flag() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 6).await;
    // This peer implements HTTP Responses/SSE, so register that capability via
    // the same provider configuration read by every normal CLI invocation.
    std::fs::write(
        test.home_path().join("config.toml"),
        format!(
            "model_provider = \"resume_fixture\"\n\
             [model_providers.resume_fixture]\n\
             name = \"Resume HTTP fixture\"\n\
             base_url = {}\n\
             wire_api = \"responses\"\n\
             requires_openai_auth = true\n\
             supports_websockets = false\n",
            serde_json::to_string(&format!("{}/v1", server.uri()))?,
        ),
    )?;

    let dir_a = TempDir::new()?;
    let dir_b = TempDir::new()?;

    let marker_a = format!("resume-cwd-a-{}", Uuid::new_v4());
    let prompt_a = format!("echo {marker_a}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_a.path())
        .arg(&prompt_a)
        .assert()
        .success();

    let marker_b = format!("resume-cwd-b-{}", Uuid::new_v4());
    let prompt_b = format!("echo {marker_b}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_b.path())
        .arg(&prompt_b)
        .assert()
        .success();

    let sessions_dir = test.home_path().join("sessions");
    let path_a = find_session_file_containing_marker(&sessions_dir, &marker_a)
        .expect("no session file found for marker_a");
    let path_b = find_session_file_containing_marker(&sessions_dir, &marker_b)
        .expect("no session file found for marker_b");
    assert_ne!(
        path_a, path_b,
        "different initial runs must create different sessions"
    );

    // updated_at has second granularity. Make B strictly newer than A before
    // testing that the normal cwd filter still chooses A.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let session_id_b = extract_conversation_id(&path_b);
    let marker_b_touch = format!("resume-cwd-b-touch-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_b.path())
        .arg("resume")
        .arg(&session_id_b)
        .arg(format!("echo {marker_b_touch}"))
        .assert()
        .success();
    assert_eq!(
        find_session_file_containing_marker(&sessions_dir, &marker_b_touch),
        Some(path_b.clone())
    );

    // Make the filtered A turn strictly newer than B for the following --all
    // assertion, independently of UUID tie ordering on fast machines.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let marker_a2 = format!("resume-cwd-a-filtered-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_a.path())
        .arg("resume")
        .arg("--last")
        .arg(format!("echo {marker_a2}"))
        .assert()
        .success();
    assert_eq!(
        find_session_file_containing_marker(&sessions_dir, &marker_a2),
        Some(path_a.clone()),
        "resume --last must filter out the newer session in another cwd"
    );
    assert!(!std::fs::read_to_string(&path_b)?.contains(&marker_a2));

    let marker_all = format!("resume-cwd-all-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_b.path())
        .arg("resume")
        .arg("--last")
        .arg("--all")
        .arg(format!("echo {marker_all}"))
        .assert()
        .success();
    assert_eq!(
        find_session_file_containing_marker(&sessions_dir, &marker_all),
        Some(path_a.clone()),
        "resume --last --all must choose the newest session even in another cwd"
    );
    assert!(!std::fs::read_to_string(&path_b)?.contains(&marker_all));

    // Resuming A from B records B as A's latest turn cwd, so filtered lookup
    // from B must now discover A rather than only considering its initial cwd.
    let marker_latest_cwd = format!("resume-latest-cwd-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(dir_b.path())
        .arg("resume")
        .arg("--last")
        .arg(format!("echo {marker_latest_cwd}"))
        .assert()
        .success();
    assert_eq!(
        find_session_file_containing_marker(&sessions_dir, &marker_latest_cwd),
        Some(path_a),
        "cwd filtering must use the latest recorded turn context"
    );
    assert!(!std::fs::read_to_string(&path_b)?.contains(&marker_latest_cwd));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_accepts_global_flags_after_subcommand() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;

    // Seed a session.
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("echo seed-resume-session")
        .assert()
        .success();

    // Resume while passing global flags after the subcommand to ensure clap accepts them.
    let base = format!("{}/v1", server.uri());
    let base_config = format!("openai_base_url={}", serde_json::to_string(&base)?);
    test.cmd()
        .arg("resume")
        .arg("--last")
        .arg("--config")
        .arg(base_config)
        .arg("--json")
        .arg("--model")
        .arg("gpt-5.2-codex")
        .arg("--config")
        .arg("reasoning_level=xhigh")
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .arg("--skip-git-repo-check")
        .arg("echo resume-with-global-flags-after-subcommand")
        .assert()
        .success();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_includes_output_schema_in_request() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let response_mock = mount_exec_responses(&server, /*count*/ 2).await;

    let schema_contents = serde_json::json!({
        "type": "object",
        "properties": {
            "answer": { "type": "string" }
        },
        "required": ["answer"],
        "additionalProperties": false
    });
    let schema_path = test.cwd_path().join("schema.json");
    std::fs::write(&schema_path, serde_json::to_vec_pretty(&schema_contents)?)?;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("echo seed-resume-session")
        .assert()
        .success();

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("resume")
        .arg("--last")
        .arg("--json")
        .arg("--output-schema")
        .arg(&schema_path)
        .arg("echo resume-with-schema")
        .assert()
        .success();

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    let payload: Value = requests[1].body_json();
    let text = payload.get("text").expect("request missing text field");
    let format = text
        .get("format")
        .expect("request missing text.format field");
    assert_eq!(
        format,
        &serde_json::json!({
            "name": "codex_output_schema",
            "type": "json_schema",
            "strict": true,
            "schema": schema_contents,
        })
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_by_id_appends_to_existing_file() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let repo_root = exec_repo_root()?;

    // 1) First run: create a session
    let marker = format!("resume-by-id-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");
    let session_id = extract_conversation_id(&path);
    assert!(
        !session_id.is_empty(),
        "missing conversation id in meta line"
    );

    // 2) Resume by id
    let marker2 = format!("resume-by-id-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt2)
        .arg("resume")
        .arg(&session_id)
        .assert()
        .success();

    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no resumed session file containing marker2");
    assert_eq!(
        resumed_path, path,
        "resume by id should append to existing file"
    );
    let content = std::fs::read_to_string(&resumed_path)?;
    assert!(content.contains(&marker));
    assert!(content.contains(&marker2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_preserves_cli_configuration_overrides() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let repo_root = exec_repo_root()?;

    let marker = format!("resume-config-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--sandbox")
        .arg("workspace-write")
        .arg("--model")
        .arg("gpt-5.1")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");

    let marker2 = format!("resume-config-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");

    let output = test
        .cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--sandbox")
        .arg("workspace-write")
        .arg("--model")
        .arg("gpt-5.1-high")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt2)
        .arg("resume")
        .arg("--last")
        .output()
        .context("resume run should succeed")?;

    assert!(output.status.success(), "resume run failed: {output:?}");

    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        stderr.contains("model: gpt-5.1-high"),
        "stderr missing model override: {stderr}"
    );
    assert!(
        stderr.contains("sandbox: read-only"),
        "stderr missing downgraded sandbox note: {stderr}"
    );

    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no resumed session file containing marker2");
    assert_eq!(resumed_path, path, "resume should append to same file");

    let content = std::fs::read_to_string(&resumed_path)?;
    assert!(content.contains(&marker));
    assert!(content.contains(&marker2));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_accepts_images_after_subcommand() -> anyhow::Result<()> {
    skip_if_no_network!(Ok(()));

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let repo_root = exec_repo_root()?;

    let marker = format!("resume-image-{}", Uuid::new_v4());
    let prompt = format!("echo {marker}");

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg(&prompt)
        .assert()
        .success();

    let image_path = test.cwd_path().join("resume_image.png");
    let image_path_2 = test.cwd_path().join("resume_image_2.png");
    let image_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    std::fs::write(&image_path, image_bytes)?;
    std::fs::write(&image_path_2, image_bytes)?;

    let marker2 = format!("resume-image-2-{}", Uuid::new_v4());
    let prompt2 = format!("echo {marker2}");
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(&repo_root)
        .arg("resume")
        .arg("--last")
        .arg("--image")
        .arg(&image_path)
        .arg("--image")
        .arg(&image_path_2)
        .arg(&prompt2)
        .assert()
        .success();

    let sessions_dir = test.home_path().join("sessions");
    let resumed_path = find_session_file_containing_marker(&sessions_dir, &marker2)
        .expect("no session file found after resume with images");
    let image_count = max_user_image_count(&resumed_path);
    assert_eq!(
        image_count, 2,
        "resume prompt should include both attached images"
    );

    Ok(())
}
