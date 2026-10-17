//! Host crash containment and runtime-replacement reporting (validation T2).
//!
//! One test function per binary: the host processes are found as children of
//! this test process, so no other test may spawn hosts concurrently.
#![cfg(windows)]
#![allow(clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::NoopCodeModeSessionDelegate;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::WaitOutcome;
use codex_code_mode::WaitRequest;

fn request(source: &str, state_path: Option<&PathBuf>) -> ExecuteRequest {
    ExecuteRequest {
        state_path: state_path.cloned(),
        tool_call_id: "crash".to_string(),
        enabled_tools: Vec::new().into(),
        source: source.to_string(),
        yield_time_ms: Some(60_000),
        max_output_tokens: None,
        default_tool_timeout_ms: None,
    }
}

/// Runs a cell to its first response: its text on success, or the error a
/// model would see.
async fn run(
    session: &Arc<dyn CodeModeSession>,
    source: &str,
    state_path: Option<&PathBuf>,
) -> Result<String, String> {
    let started = session.execute(request(source, state_path)).await?;
    match started.initial_response().await? {
        RuntimeResponse::Result {
            content_items,
            error_text: None,
            ..
        } => Ok(content_items
            .into_iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } => Some(text),
                FunctionCallOutputContentItem::InputImage { .. } => None,
            })
            .collect()),
        RuntimeResponse::Result {
            error_text: Some(error),
            ..
        } => Err(error),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Starts a cell that stays live until its host goes away.
async fn start_live_cell(session: &Arc<dyn CodeModeSession>) -> CellId {
    let started = session
        .execute(request(
            "await yield_control(); await new Promise(resolve => setTimeout(resolve, 3_600_000));",
            None,
        ))
        .await
        .expect("start live cell");
    let cell_id = started.cell_id.clone();
    assert!(matches!(
        started.initial_response().await.expect("live cell response"),
        RuntimeResponse::ExplicitYield { .. }
    ));
    cell_id
}

fn child_host_pids() -> Vec<u32> {
    let filter = format!(
        "ParentProcessId={} AND Name='codex-code-mode-host.exe'",
        std::process::id()
    );
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("Get-CimInstance Win32_Process -Filter \"{filter}\" | ForEach-Object {{ $_.ProcessId }}"),
        ])
        .output()
        .expect("list host processes");
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

/// Kills this test's only host and waits until the live cell observes it.
async fn kill_host(session: &Arc<dyn CodeModeSession>, live_cell: CellId) {
    let pids = child_host_pids();
    assert_eq!(pids.len(), 1, "expected exactly one host child: {pids:?}");
    let status = std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pids[0].to_string()])
        .output()
        .expect("taskkill host");
    assert!(status.status.success(), "taskkill failed: {status:?}");
    let observed = tokio::time::timeout(
        Duration::from_secs(30),
        session.wait(WaitRequest {
            cell_id: live_cell,
            yield_time_ms: 60_000,
            recovery: None,
        }),
    )
    .await
    .expect("live cell must observe the host exit");
    assert!(
        !matches!(observed, Ok(WaitOutcome::LiveCell(RuntimeResponse::ExplicitYield { .. }))),
        "a cell of a killed host cannot still be live: {observed:?}"
    );
}

fn assert_replacement_notice(error: &str) {
    for expected in ["runtime was replaced", "lost", "did not run", "persist"] {
        assert!(error.contains(expected), "missing `{expected}` in notice: {error}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_host_reports_lost_state_before_running_more_code() {
    let program = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary");

    // In-memory values cannot survive the host; the first cell on the
    // replacement runtime must not run as if they had never existed.
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(program.clone());
    let session = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("create session");
    assert_eq!(run(&session, r#"store("memory", 2); text("stored");"#, None).await, Ok("stored".to_string()));
    let live = start_live_cell(&session).await;
    kill_host(&session, live).await;
    let first = run(&session, r#"text(String(load("memory")));"#, None).await;
    assert_replacement_notice(&first.expect_err("the first cell after a replacement must not run"));
    assert_eq!(
        run(&session, r#"text(String(load("memory")));"#, None).await,
        Ok("undefined".to_string()),
        "after the notice, the replacement runtime starts empty"
    );
    session.shutdown().await.expect("shutdown");
    drop(session);
    drop(provider);

    // Values committed with `persist` are restored by the next persist cell.
    let directory = tempfile::tempdir().expect("tempdir");
    let state_path = directory.path().join("state.json");
    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(program);
    let session = provider
        .create_session(Arc::new(NoopCodeModeSessionDelegate))
        .await
        .expect("create session");
    assert_eq!(
        run(&session, r#"store("durable", 1); text("stored");"#, Some(&state_path)).await,
        Ok("stored".to_string())
    );
    let live = start_live_cell(&session).await;
    kill_host(&session, live).await;
    let first = run(&session, r#"text(String(load("durable")));"#, Some(&state_path)).await;
    assert_replacement_notice(&first.expect_err("the first cell after a replacement must not run"));
    assert_eq!(
        run(&session, r#"text(String(load("durable")));"#, Some(&state_path)).await,
        Ok("1".to_string()),
        "persisted values return with the next persist cell"
    );
    session.shutdown().await.expect("shutdown");
}
