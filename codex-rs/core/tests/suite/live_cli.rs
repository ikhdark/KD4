//! Optional smoke tests that hit the real OpenAI /v1/responses endpoint. They are `#[ignore]` by
//! default so CI stays deterministic and free. Developers can run them locally with
//! `just core-test core_cli_workspace --run-ignored only live_cli` provided they set a valid
//! `OPENAI_API_KEY`.

use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

fn require_api_key() -> String {
    std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY env var not set — live test cannot run")
}

fn resolve_codex_binary() -> Result<PathBuf, codex_utils_cargo_bin::CargoBinError> {
    codex_utils_cargo_bin::cargo_bin("codex")
}

/// Runs the real CLI with live output and the same contained capture path as
/// hermetic subprocess tests. No output-reader thread can outlive the command.
fn run_live(prompt: &str) -> anyhow::Result<(assert_cmd::assert::Assert, TempDir)> {
    let dir = TempDir::new()?;
    let home = TempDir::new()?;
    let codex_home = home.path().join(".codex");
    std::fs::create_dir_all(&codex_home)?;
    let mut command = tokio::process::Command::new(resolve_codex_binary()?);
    command
        .current_dir(dir.path())
        .env("OPENAI_API_KEY", require_api_key())
        .env("HOME", home.path())
        .env("CODEX_HOME", &codex_home)
        .arg("exec")
        .arg("--skip-git-repo-check")
        .arg("--")
        .arg(prompt);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let output = runtime.block_on(core_test_support::process::capture_contained_command(
        &mut command,
        Duration::from_secs(5 * 60),
        /*mirror_output*/ true,
    )).map_err(|error| anyhow::anyhow!("live codex CLI should finish within its process/output deadline: {error}"))?;
    Ok((output.assert(), dir))
}

#[ignore = "requires OPENAI_API_KEY and sends paid requests to the live OpenAI API"]
#[test]
fn live_create_file_hello_txt() -> anyhow::Result<()> {
    let (assert, dir) = run_live(
        "Use the shell tool with the apply_patch command to create a file named hello.txt containing the text 'hello'.",
    )?;

    assert.success();

    let path = dir.path().join("hello.txt");
    assert!(path.exists(), "hello.txt was not created by the model");

    let contents = std::fs::read_to_string(path)?;

    assert_eq!(contents.trim(), "hello");
    Ok(())
}

#[ignore = "requires OPENAI_API_KEY and sends paid requests to the live OpenAI API"]
#[test]
fn live_print_working_directory() -> anyhow::Result<()> {
    let (assert, dir) = run_live("Print the current working directory using the shell function.")?;

    assert
        .success()
        .stdout(predicate::str::contains(dir.path().to_string_lossy()));
    Ok(())
}

#[tokio::test]
async fn run_codex_times_out() {
    let command = |sleep: bool| {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command.arg("--exact").arg("suite::live_cli::timeout_test_child").arg("--nocapture")
            .env_remove("CODEX_LIVE_CLI_TIMEOUT_CHILD");
        if sleep {
            command.env("CODEX_LIVE_CLI_TIMEOUT_CHILD", "1");
        }
        command
    };
    let output = core_test_support::process::capture_contained_command(
        &mut command(false), Duration::from_secs(30), false,
    ).await.expect("exiting child should finish");
    assert!(output.status.success());
    let started = Instant::now();
    let error = core_test_support::process::capture_contained_command(
        &mut command(true), Duration::from_millis(100), false,
    ).await.expect_err("sleeping child must time out");
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn timeout_test_child() {
    if std::env::var_os("CODEX_LIVE_CLI_TIMEOUT_CHILD").is_some() {
        std::thread::sleep(Duration::from_secs(30));
    }
}

#[cfg(windows)]
#[tokio::test]
async fn contained_capture_terminates_descendants_holding_both_pipes() {
    for wait_in_root in [false, true] {
        let dir = TempDir::new().unwrap();
        let pid_path = dir.path().join("descendant.pid");
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command.arg("--exact").arg("suite::live_cli::pipe_holding_test_child").arg("--nocapture")
            .env("CODEX_PIPE_HOLDER_ROLE", "root")
            .env("CODEX_PIPE_HOLDER_PID", &pid_path)
            .env("CODEX_PIPE_HOLDER_WAIT", if wait_in_root { "1" } else { "0" });
        let started = Instant::now();
        let result = core_test_support::process::capture_contained_command(
            &mut command, Duration::from_secs(5), false,
        ).await;
        let stdout_line = format!("stdout:{}", "x".repeat(12_000));
        let stderr_line = format!("stderr:{}", "y".repeat(12_000));
        if wait_in_root {
            let error = result.expect_err("root and inherited pipes must time out");
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            let diagnostics = error.to_string();
            assert!(diagnostics.contains(&stdout_line));
            assert!(diagnostics.contains(&stderr_line));
        } else {
            let output = result.expect("root exit must close descendant pipes");
            assert!(output.status.success());
            assert!(String::from_utf8(output.stdout).unwrap().lines().any(|line| line == stdout_line));
            assert!(String::from_utf8(output.stderr).unwrap().lines().any(|line| line == stderr_line));
        }
        assert!(started.elapsed() < Duration::from_secs(15));
        let pid = std::fs::read_to_string(&pid_path).expect("descendant was spawned");
        core_test_support::process::wait_for_process_exit(&pid).await.unwrap();
    }
}

#[cfg(windows)]
#[test]
fn pipe_holding_test_child() {
    match std::env::var("CODEX_PIPE_HOLDER_ROLE").as_deref() {
        Ok("root") => {
            let descendant = Command::new(std::env::current_exe().unwrap())
                .arg("--exact").arg("suite::live_cli::pipe_holding_test_child").arg("--nocapture")
                .env("CODEX_PIPE_HOLDER_ROLE", "descendant")
                .stdin(Stdio::null()).stdout(Stdio::inherit()).stderr(Stdio::inherit())
                .spawn().unwrap();
            std::fs::write(std::env::var_os("CODEX_PIPE_HOLDER_PID").unwrap(), descendant.id().to_string()).unwrap();
            println!("stdout:{}", "x".repeat(12_000));
            eprintln!("stderr:{}", "y".repeat(12_000));
            if std::env::var("CODEX_PIPE_HOLDER_WAIT").as_deref() == Ok("1") {
                std::thread::sleep(Duration::from_secs(30));
            }
            // The parent capture owns the entire Job, even after this root exits.
            drop(descendant);
        }
        Ok("descendant") => std::thread::sleep(Duration::from_secs(30)),
        _ => {}
    }
}
