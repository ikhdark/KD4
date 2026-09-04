use anyhow::Result;
use app_test_support::BlockedCompletionProofFixture;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::write_mock_responses_config_toml;
use std::collections::BTreeMap;
#[cfg(windows)]
use std::process::Stdio;
use tempfile::TempDir;
#[cfg(windows)]
use tokio::process::Child;
#[cfg(windows)]
use tokio::process::Command;
#[cfg(windows)]
use tokio::time::Duration;
#[cfg(windows)]
use tokio::time::timeout;
#[cfg(windows)]
use wiremock::Mock;
#[cfg(windows)]
use wiremock::MockServer;
#[cfg(windows)]
use wiremock::ResponseTemplate;
#[cfg(windows)]
use wiremock::matchers::method;
#[cfg(windows)]
use wiremock::matchers::path_regex;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_proof_block_hides_success_and_fails_real_cli_exec() -> Result<()> {
    let fixture = BlockedCompletionProofFixture::new()?;
    let server = create_mock_responses_server_repeating_assistant("premature success").await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::new(),
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;

    let mut command = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    let output = command
        .env("CODEX_HOME", codex_home.path())
        .current_dir(fixture.repo_path())
        .args([
            "exec",
            "--skip-git-repo-check",
            "finish without running certification",
        ])
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "blocked terminal completion unexpectedly succeeded; stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        !stdout.contains("premature success") && !stderr.contains("premature success"),
        "CLI exposed the buffered assistant success; stdout={stdout:?}, stderr={stderr:?}"
    );
    assert!(
        stderr.contains("CompletionProofGate blocked terminal success"),
        "CLI did not surface the completion-proof failure; stderr={stderr:?}"
    );
    assert!(
        !fixture.canonical_runner_launched(),
        "CLI must not launch canonical certification"
    );

    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_killed_real_cli_exec_preserves_exact_physical_credential() -> Result<()> {
    let fixture = BlockedCompletionProofFixture::new()?;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_delay(Duration::from_secs(60))
                .set_body_string("data: {}\n\n"),
        )
        .mount(&server)
        .await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &server.uri(),
        &BTreeMap::new(),
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;
    let physical_credential_before =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            codex_home.path(),
            fixture.repo_path(),
        )
        .map_err(anyhow::Error::msg)?;

    let mut child = Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .env("CODEX_HOME", codex_home.path())
        .current_dir(fixture.repo_path())
        .args([
            "exec",
            "--skip-git-repo-check",
            "remain active until hard-killed",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    timeout(Duration::from_secs(30), async {
        loop {
            if server
                .received_requests()
                .await
                .is_some_and(|requests| !requests.is_empty())
            {
                return Ok::<(), anyhow::Error>(());
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("CLI exited before hard-kill readiness with {status}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    hard_kill_and_wait(&mut child).await?;

    let physical_credential_after =
        codex_core::test_support::completion_proof_physical_credential_snapshot(
            codex_home.path(),
            fixture.repo_path(),
        )
        .map_err(anyhow::Error::msg)?;
    assert_eq!(
        physical_credential_after, physical_credential_before,
        "hard-killed CLI changed the exact physical completion-proof credential"
    );
    Ok(())
}

#[cfg(windows)]
async fn hard_kill_and_wait(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    child.start_kill()?;
    child.wait().await
}
