#![allow(clippy::unwrap_used)]
use anyhow::Context;
use assert_cmd::assert::Assert;
use core_test_support::process::capture_contained_command;
use core_test_support::require_network;
use core_test_support::responses;
use core_test_support::test_codex_exec::test_codex_exec;
use predicates::str::contains;
use pretty_assertions::assert_eq;
use serde_json::Value;
use std::string::ToString;
use std::time::Duration;
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
async fn extract_conversation_id(path: &std::path::Path) -> String {
    let (_, thread_id, parse_errors) = codex_core::RolloutRecorder::load_rollout_items(path)
        .await
        .expect("read rollout");
    assert_eq!(parse_errors, 0, "invalid rollout records");
    thread_id
        .expect("missing conversation id in meta line")
        .to_string()
}

async fn read_rollout_records(path: &std::path::Path) -> anyhow::Result<Vec<Value>> {
    // Session metadata may be externalized. Hydrate every record before editing or
    // copying it to another home, preserving timestamps and persisted settings.
    let mut reader = codex_rollout::open_rollout_line_reader(path).await?;
    let mut records = Vec::new();
    while let Some(line) = reader.next_line().await? {
        if !line.trim().is_empty() {
            records.push(serde_json::from_str(&line)?);
        }
    }
    Ok(records)
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

async fn contained_exec_assert(
    command: &mut tokio::process::Command,
    stage: &str,
) -> anyhow::Result<Assert> {
    // Preserve the assertion API while bounding both process exit and inherited
    // output pipes. A deadline failure includes the output captured so far.
    let output = capture_contained_command(command, Duration::from_secs(30), false)
        .await
        .with_context(|| format!("codex-exec {stage}"))?;
    Ok(Assert::new(output))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn structured_finals_fail_locally_and_preserve_previous_artifacts() -> anyhow::Result<()> {
    require_network!();
    for answer in [
        "exec response",
        "{}",
        r#"{"answer":1}"#,
        r#"{"answer":"ok"} trailing"#,
    ] {
        let test = test_codex_exec();
        let server = MockServer::start().await;
        let mock = responses::mount_sse_sequence(
            &server,
            vec![responses::sse(vec![
                responses::ev_response_created("invalid-final"),
                responses::ev_assistant_message("invalid-message", answer),
                responses::ev_completed("invalid-final"),
            ])],
        )
        .await;
        let schema = test.cwd_path().join("schema.json");
        let artifact = test.cwd_path().join("answer.json");
        std::fs::write(
            &schema,
            r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#,
        )?;
        std::fs::write(&artifact, "previous valid answer")?;
        let output = test
            .cmd_with_server(&server)
            .args(["--skip-git-repo-check", "--json", "--output-schema"])
            .arg(&schema)
            .arg("-o")
            .arg(&artifact)
            .arg("return the answer")
            .assert()
            .failure()
            .get_output()
            .clone();
        let stdout = String::from_utf8(output.stdout)?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(1),
            "unexpected process failure: {stderr}"
        );
        let events: Vec<Value> = stdout
            .lines()
            .map(serde_json::from_str)
            .collect::<Result<_, _>>()?;
        assert_eq!(
            events
                .iter()
                .filter(|event| event["type"] == "turn.failed")
                .count(),
            1,
            "stdout: {stdout}\nstderr: {stderr}"
        );
        assert!(!events.iter().any(|event| event["type"] == "turn.completed"));
        assert_eq!(std::fs::read_to_string(&artifact)?, "previous valid answer");
        assert_eq!(mock.requests().len(), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_schema_and_empty_tasks_start_no_inference() -> anyhow::Result<()> {
    require_network!();
    for (case_index, (prompt, schema)) in [
        ("", None),
        ("\u{2003}\t", None),
        ("return", Some(r#"{"type":"not-a-type"}"#)),
        ("return", Some(r#"{"required":"answer"}"#)),
        ("return", Some("17")),
        (
            "return",
            Some(r#"{"$ref":"https://example.invalid/schema"}"#),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let test = test_codex_exec();
        let server = MockServer::start().await;
        let mut command = test.contained_cmd_with_server(&server);
        command.args(["--skip-git-repo-check", "--json"]);
        if let Some(schema) = schema {
            let path = test.cwd_path().join("schema.json");
            std::fs::write(&path, schema)?;
            command.arg("--output-schema").arg(path);
        }
        command.arg(prompt);
        contained_exec_assert(&mut command, &format!("invalid input case {case_index}"))
            .await?
            .failure();
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    Ok(())
}

async fn mount_exec_responses(
    server: &MockServer,
    count: usize,
) -> core_test_support::responses::ResponseMock {
    responses::mount_sse_sequence(server, (0..count).map(exec_sse_response).collect()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_fails_when_history_is_empty() -> anyhow::Result<()> {
    require_network!();

    let test = test_codex_exec();
    let server = MockServer::start().await;

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("resume")
        .arg("--last")
        .arg("echo should not start a new thread")
        .assert()
        .failure()
        .stderr(contains("no eligible resumable session found"));

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

#[test]
fn ephemeral_resume_rejects_both_flag_orders_before_initialization() {
    for args in [
        vec!["--ephemeral", "resume", "--last", "continue"],
        vec!["resume", "--ephemeral", "--last", "continue"],
        vec![
            "resume",
            "12345678-1234-4234-8234-123456789abc",
            "--ephemeral",
            "continue",
        ],
    ] {
        let test = test_codex_exec();
        std::fs::write(test.home_path().join("environments.toml"), "invalid = [").unwrap();
        test.cmd()
            .args(args)
            .assert()
            .failure()
            .stderr(contains("--ephemeral cannot be used with resume"));
        assert!(!test.home_path().join("sessions").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_resume_skips_newer_v2_child_and_preserves_v1_continuation() -> anyhow::Result<()>
{
    require_network!();
    for version in ["v2", "v1"] {
        let test = test_codex_exec();
        let server = MockServer::start().await;
        let mock = mount_exec_responses(&server, 2).await;
        test.cmd_with_server(&server)
            .args(["--skip-git-repo-check", "seed-eligible-root"])
            .assert()
            .success();
        let root_path = find_session_file_containing_marker(
            &test.home_path().join("sessions"),
            "seed-eligible-root",
        )
        .expect("root rollout");
        let root_id = extract_conversation_id(&root_path).await;
        let child_id = Uuid::now_v7().to_string();
        let agent_path = codex_protocol::AgentPath::root()
            .join("worker")
            .expect("worker path");
        let filename = root_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .replace(&root_id, &child_id);
        let child_path = root_path.with_file_name(filename);
        let mut records = read_rollout_records(&root_path).await?;
        let meta = records
            .iter_mut()
            .find(|record| record["type"] == "session_meta")
            .expect("canonical session metadata");
        meta["payload"]["id"] = child_id.clone().into();
        meta["payload"]["parent_thread_id"] = root_id.clone().into();
        meta["payload"]["agent_path"] = agent_path.to_string().into();
        meta["payload"]["source"] = serde_json::json!({
            "subagent":{"thread_spawn":{"parent_thread_id":root_id,"depth":1,"agent_path":agent_path}}
        });
        meta["payload"]["multi_agent_version"] = version.into();
        std::fs::write(
            &child_path,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )?;
        assert_eq!(extract_conversation_id(&child_path).await, child_id);
        std::fs::File::options()
            .write(true)
            .open(&child_path)?
            .set_times(
                std::fs::FileTimes::new().set_modified(
                    std::time::SystemTime::now() + std::time::Duration::from_secs(60),
                ),
            )?;
        let output = test
            .cmd_with_server(&server)
            .args([
                "--skip-git-repo-check",
                "--json",
                "resume",
                "--last",
                "continue-selected",
            ])
            .assert()
            .success()
            .get_output()
            .clone();
        let started: Value =
            serde_json::from_str(String::from_utf8(output.stdout)?.lines().next().unwrap())?;
        assert_eq!(
            started["thread_id"],
            if version == "v2" {
                root_id.as_str()
            } else {
                child_id.as_str()
            }
        );
        assert_eq!(mock.requests().len(), 2);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_absence_diagnostic_counts_only_eligible_history() -> anyhow::Result<()> {
    require_network!();
    let seed = test_codex_exec();
    let server = MockServer::start().await;
    let mock = mount_exec_responses(&server, 1).await;
    seed.cmd_with_server(&server)
        .args(["--skip-git-repo-check", "seed-worker-history"])
        .assert()
        .success();
    let seed_path = find_session_file_containing_marker(
        &seed.home_path().join("sessions"),
        "seed-worker-history",
    )
    .unwrap();
    let seed_records = read_rollout_records(&seed_path).await?;
    let seed_id = extract_conversation_id(&seed_path).await;
    for (version, expected) in [
        ("v2", "no eligible resumable session found"),
        (
            "v1",
            "resumable sessions were found, but none match model provider `alternate`",
        ),
    ] {
        let test = test_codex_exec();
        let target = test
            .home_path()
            .join(seed_path.strip_prefix(seed.home_path())?);
        std::fs::create_dir_all(target.parent().unwrap())?;
        let mut records = seed_records.clone();
        let meta = records
            .iter_mut()
            .find(|record| record["type"] == "session_meta")
            .expect("canonical session metadata");
        let parent_thread_id = Uuid::now_v7();
        meta["payload"]["parent_thread_id"] = parent_thread_id.to_string().into();
        meta["payload"]["source"] = serde_json::json!({
            "subagent": {"thread_spawn": {"parent_thread_id": parent_thread_id, "depth": 1}}
        });
        meta["payload"]["multi_agent_version"] = version.into();
        std::fs::write(
            &target,
            records
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )?;
        assert_eq!(extract_conversation_id(&target).await, seed_id);
        test.cmd_with_server(&server).args(["-c", &format!(
            "model_providers.alternate={{ name = 'Alternate', base_url = '{}/v1', wire_api = 'responses' }}", server.uri()),
            "-c", "model_provider='alternate'", "--skip-git-repo-check", "resume", "--last", "continue"])
            .assert().failure().stderr(contains(expected));
    }
    assert_eq!(
        mock.requests().len(),
        1,
        "rejected resume must not send a model request"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_resume_uses_settings_only_cwd_update() -> anyhow::Result<()> {
    use std::io::Write;
    require_network!();
    let test = test_codex_exec();
    let server = MockServer::start().await;
    let mock = mount_exec_responses(&server, 2).await;
    let first = TempDir::new()?;
    let latest = TempDir::new()?;
    test.cmd_with_server(&server)
        .args(["--skip-git-repo-check", "-C"])
        .arg(first.path())
        .arg("settings-only-seed")
        .assert()
        .success();
    let path = find_session_file_containing_marker(
        &test.home_path().join("sessions"),
        "settings-only-seed",
    )
    .expect("seed rollout");
    let id = extract_conversation_id(&path).await;
    let records: Vec<Value> = std::fs::read_to_string(&path)?
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let mut settings = records
        .iter()
        .rev()
        .find(|record| {
            record["type"] == "event_msg" && record["payload"]["type"] == "thread_settings_applied"
        })
        .expect("accepted settings snapshot")
        .clone();
    settings["payload"]["thread_settings"]["cwd"] = serde_json::to_value(latest.path())?;
    // Legacy snapshots carry cwd directly. Omitting environments exercises the
    // canonical reducer's documented legacy fallback rather than retaining A.
    settings["payload"]["thread_settings"]
        .as_object_mut()
        .unwrap()
        .remove("environments");
    let mut file = std::fs::OpenOptions::new().append(true).open(&path)?;
    writeln!(file, "{settings}")?;
    file.sync_all()?;
    let output = test
        .cmd_with_server(&server)
        .args(["--skip-git-repo-check", "--json", "-C"])
        .arg(latest.path())
        .args(["resume", "--last", "continue-settings-only"])
        .assert()
        .success()
        .get_output()
        .clone();
    let started: Value =
        serde_json::from_str(String::from_utf8(output.stdout)?.lines().next().unwrap())?;
    assert_eq!(started["thread_id"], id);
    assert_eq!(mock.requests().len(), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_reports_when_provider_filter_excludes_history() -> anyhow::Result<()> {
    require_network!();

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
    require_network!();

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
    require_network!();

    let test = test_codex_exec();
    let server = MockServer::start().await;

    let mut command = test.contained_cmd_with_server(&server);
    command
        .arg("--skip-git-repo-check")
        .arg("resume")
        .arg("missing-thread-name")
        .arg("echo should not start a new thread");
    contained_exec_assert(&mut command, "unknown resume name")
        .await?
        .failure()
        .stderr(contains(
            "no resumable session named `missing-thread-name` found",
        ));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_by_name_finds_sqlite_only_title_with_canonical_cwd() -> anyhow::Result<()> {
    require_network!();

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 2).await;
    let session_cwd = TempDir::new()?;
    let other_cwd = TempDir::new()?;

    let marker = format!("resume-sqlite-title-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(session_cwd.path())
        .arg(format!("echo {marker}"))
        .assert()
        .success();
    let sessions_dir = test.home_path().join("sessions");
    let path = find_session_file_containing_marker(&sessions_dir, &marker)
        .expect("no session file found after first run");

    // A committed rename whose compatibility name-index append failed leaves the title only in
    // SQLite. Pin the canonical cwd that metadata writers store; exec's configured cwd need not
    // match it textually (verbatim prefix on Windows, resolved symlinks on macOS).
    let title = format!("sqlite-only-title-{}", Uuid::new_v4());
    {
        use sqlx::Connection;
        let mut connection = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new()
                .filename(codex_state::state_db_path(test.home_path())),
        )
        .await?;
        let canonical_cwd =
            codex_rollout::state_integration::normalize_cwd_for_state_db(session_cwd.path());
        let changed = sqlx::query("UPDATE threads SET title = ?, cwd = ? WHERE id = ?")
            .bind(title.as_str())
            .bind(canonical_cwd.display().to_string())
            .bind(extract_conversation_id(&path).await)
            .execute(&mut connection)
            .await?;
        assert_eq!(changed.rows_affected(), 1, "fixture thread must be indexed");
        connection.close().await?;
    }
    let name_index =
        std::fs::read_to_string(test.home_path().join("session_index.jsonl")).unwrap_or_default();
    assert!(
        !name_index.contains(&title),
        "the title must exist only in SQLite"
    );

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(other_cwd.path())
        .arg("resume")
        .arg(&title)
        .arg("echo should not start a new thread")
        .assert()
        .failure()
        .stderr(contains(format!(
            "a resumable session named `{title}` was found"
        )))
        .stderr(contains("use --all"));

    let marker2 = format!("resume-sqlite-title-2-{}", Uuid::new_v4());
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("-C")
        .arg(session_cwd.path())
        .arg("resume")
        .arg(&title)
        .arg(format!("echo {marker2}"))
        .assert()
        .success();
    assert_eq!(
        find_session_file_containing_marker(&sessions_dir, &marker2),
        Some(path),
        "resume by SQLite title must append to the titled thread"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_last_appends_to_existing_file() -> anyhow::Result<()> {
    require_network!();

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
    require_network!();

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
    require_network!();

    async fn backdate_session(
        home: &std::path::Path,
        rollout: &std::path::Path,
    ) -> anyhow::Result<()> {
        use sqlx::Connection;
        let timestamp = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        let seconds = i64::try_from(timestamp.duration_since(std::time::UNIX_EPOCH)?.as_secs())?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(rollout)?
            .set_times(std::fs::FileTimes::new().set_modified(timestamp))?;
        let mut connection = sqlx::SqliteConnection::connect_with(
            &sqlx::sqlite::SqliteConnectOptions::new().filename(codex_state::state_db_path(home)),
        )
        .await?;
        let changed =
            sqlx::query("UPDATE threads SET updated_at = ?, updated_at_ms = ? WHERE id = ?")
                .bind(seconds)
                .bind(seconds * 1000)
                .bind(extract_conversation_id(rollout).await)
                .execute(&mut connection)
                .await?;
        assert_eq!(changed.rows_affected(), 1, "fixture thread must be indexed");
        connection.close().await?;
        Ok(())
    }

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let _response_mock = mount_exec_responses(&server, /*count*/ 6).await;

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

    // Make B strictly newer without depending on timestamp precision or speed.
    backdate_session(test.home_path(), &path_a).await?;
    let session_id_b = extract_conversation_id(&path_b).await;
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

    // Keep both filesystem fallback and SQLite ordering consistent for --all.
    backdate_session(test.home_path(), &path_b).await?;
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
    require_network!();

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let response_mock = mount_exec_responses(&server, /*count*/ 2).await;

    // Seed a session.
    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("echo seed-resume-session")
        .assert()
        .success();

    // Resume while passing global flags after the subcommand to ensure clap accepts them.
    test.cmd_with_server(&server)
        .arg("resume")
        .arg("--last")
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

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[1].body_json()["model"], "gpt-5.2-codex");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_resume_includes_output_schema_in_request() -> anyhow::Result<()> {
    require_network!();

    let test = test_codex_exec();
    let server = MockServer::start().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            exec_sse_response(0),
            responses::sse(vec![
                responses::ev_response_created("structured"),
                responses::ev_assistant_message("structured-message", r#"{"answer":"ok"}"#),
                responses::ev_completed("structured"),
            ]),
        ],
    )
    .await;

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

    let mut command = test.contained_cmd_with_server(&server);
    command
        .arg("--skip-git-repo-check")
        .arg("echo seed-resume-session");
    contained_exec_assert(&mut command, "seed output-schema session")
        .await?
        .success();

    let mut command = test.contained_cmd_with_server(&server);
    command
        .arg("--skip-git-repo-check")
        .arg("resume")
        .arg("--last")
        .arg("--json")
        .arg("--output-schema")
        .arg(&schema_path)
        .arg("echo resume-with-schema");
    contained_exec_assert(&mut command, "resume with output schema")
        .await?
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
    require_network!();

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
    let session_id = extract_conversation_id(&path).await;
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
    require_network!();

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

    // Without a sandbox, Windows reports workspace-write as read-only, which is also what a
    // dropped override would print. Only a mode that differs from every default discriminates.
    let output = test
        .cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--sandbox")
        .arg("danger-full-access")
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
        stderr.contains("sandbox: danger-full-access"),
        "stderr missing sandbox override: {stderr}"
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
    require_network!();

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_images_use_piped_stdin_as_the_prompt() -> anyhow::Result<()> {
    require_network!();

    // The TypeScript SDK pipes the prompt through stdin and passes images as
    // flags without a positional prompt; stdin must remain the prompt itself.
    let test = test_codex_exec();
    let server = MockServer::start().await;
    let response_mock = mount_exec_responses(&server, /*count*/ 1).await;
    let image_path = test.cwd_path().join("stdin_prompt_image.png");
    let image_bytes: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    std::fs::write(&image_path, image_bytes)?;
    let marker = format!("image-stdin-prompt-{}", Uuid::new_v4());

    test.cmd_with_server(&server)
        .arg("--skip-git-repo-check")
        .arg("--image")
        .arg(&image_path)
        .write_stdin(format!("{marker}\n"))
        .assert()
        .success();

    let texts = response_mock.single_request().message_input_texts("user");
    assert!(
        texts.iter().any(|text| text.trim() == marker),
        "stdin should be the prompt: {texts:?}"
    );
    assert!(
        !texts.iter().any(|text| text.contains("<stdin>")),
        "stdin must not be wrapped as appended context: {texts:?}"
    );

    Ok(())
}
