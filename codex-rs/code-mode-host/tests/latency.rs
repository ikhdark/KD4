//! Host-backed Code Mode latency benchmark (validation T8). Ignored by default:
//! `cargo test -p codex-code-mode-host --test latency -- --ignored --nocapture`
//! prints one JSON line with per-scenario millisecond statistics and, on
//! Windows, the warm host's memory after the allocation-heavy cells.
#![allow(clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolDefinition;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode_protocol::NestedCancellation;
use codex_protocol::ToolName;
use serde_json::json;
use tokio_util::sync::CancellationToken;

struct EchoDelegate;

impl CodeModeSessionDelegate for EchoDelegate {
    fn invoke_tool<'a>(
        &'a self,
        _invocation: CodeModeNestedToolCall,
        _cancellation: NestedCancellation,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async { Ok(json!({ "value": "output" })) })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

const EMPTY_CELL: &str = r#"text("ok");"#;
const NESTED_CELL: &str = r#"
for (let index = 0; index < 10; index += 1) {
  await tools.echo({ index });
}
text("ok");
"#;
const CPU_CELL: &str = r#"
const values = Array.from({ length: 200000 }, (_, index) => (index * 7919) % 200003);
values.sort((left, right) => left - right);
const parsed = JSON.parse(JSON.stringify(values));
text(String(parsed.length + parsed[0] + parsed[parsed.length - 1]));
"#;
const ALLOC_CELL: &str = r#"
const objects = Array.from({ length: 500000 }, (_, index) => ({ index, label: `item-${index}` }));
text(String(objects.reduce((total, object) => total + object.label.length, 0)));
"#;

fn request(source: &str) -> ExecuteRequest {
    ExecuteRequest {
        state_path: None,
        tool_call_id: "latency".to_string(),
        enabled_tools: vec![ToolDefinition {
            name: "echo".to_string(),
            tool_name: ToolName::plain("echo"),
            description: "".into(),
            kind: CodeModeToolKind::Function,
            input_schema: None,
            default_timeout_ms: None,
            output_schema: None,
        }]
        .into(),
        source: source.to_string(),
        yield_time_ms: Some(60_000),
        max_output_tokens: None,
        default_tool_timeout_ms: None,
    }
}

async fn timed_cell(session: &Arc<dyn CodeModeSession>, source: &str) -> Duration {
    let started_at = Instant::now();
    let started = session.execute(request(source)).await.expect("start cell");
    let response = started.initial_response().await.expect("cell response");
    let elapsed = started_at.elapsed();
    match response {
        RuntimeResponse::Result {
            error_text: None, ..
        } => elapsed,
        other => panic!("benchmark cell did not complete cleanly: {other:?}"),
    }
}

/// Working set, private bytes and peak working set of the newest host child.
#[cfg(windows)]
fn host_memory() -> serde_json::Value {
    let filter = format!(
        "ParentProcessId={} AND Name='codex-code-mode-host.exe'",
        std::process::id()
    );
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Get-CimInstance Win32_Process -Filter \"{filter}\" | Sort-Object CreationDate | Select-Object -Last 1 | ForEach-Object {{ \"$($_.WorkingSetSize) $($_.PrivatePageCount) $($_.PeakWorkingSetSize)\" }}"
            ),
        ])
        .output()
        .expect("query host memory");
    let values: Vec<u64> = String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|value| value.parse().ok())
        .collect();
    match values.as_slice() {
        [working_set, private, peak_kib] => json!({
            "working_set_bytes": working_set,
            "private_bytes": private,
            "peak_working_set_bytes": peak_kib * 1024,
        }),
        _ => json!(null),
    }
}

#[cfg(not(windows))]
fn host_memory() -> serde_json::Value {
    json!(null)
}

fn summary(samples: &mut [Duration]) -> serde_json::Value {
    samples.sort();
    let millis = |duration: Duration| (duration.as_secs_f64() * 1_000.0 * 100.0).round() / 100.0;
    let percentile = |fraction: f64| {
        let index = ((samples.len() - 1) as f64 * fraction).round() as usize;
        millis(samples[index])
    };
    json!({
        "n": samples.len(),
        "min_ms": millis(samples[0]),
        "median_ms": percentile(0.5),
        "p90_ms": percentile(0.9),
        "max_ms": millis(samples[samples.len() - 1]),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "latency benchmark; run explicitly with --ignored --nocapture"]
async fn host_latency_benchmark() {
    let program = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host").expect("host binary");

    // A fresh provider owns a fresh host process: spawn, handshake, session
    // open, and the first cell's isolate startup.
    let mut cold_session = Vec::new();
    let mut cold_first_cell = Vec::new();
    for _ in 0..8 {
        let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(program.clone());
        let started_at = Instant::now();
        let session = provider
            .create_session(Arc::new(EchoDelegate))
            .await
            .expect("create session");
        cold_session.push(started_at.elapsed());
        cold_first_cell.push(timed_cell(&session, EMPTY_CELL).await);
        session.shutdown().await.expect("shutdown");
    }

    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(program);
    let session = provider
        .create_session(Arc::new(EchoDelegate))
        .await
        .expect("create session");
    for _ in 0..5 {
        timed_cell(&session, EMPTY_CELL).await;
    }
    let mut empty = Vec::new();
    for _ in 0..50 {
        empty.push(timed_cell(&session, EMPTY_CELL).await);
    }
    let mut nested = Vec::new();
    for _ in 0..30 {
        nested.push(timed_cell(&session, NESTED_CELL).await);
    }
    let mut cpu = Vec::new();
    for _ in 0..15 {
        cpu.push(timed_cell(&session, CPU_CELL).await);
    }
    let mut alloc = Vec::new();
    for _ in 0..5 {
        alloc.push(timed_cell(&session, ALLOC_CELL).await);
    }
    let memory = host_memory();
    session.shutdown().await.expect("shutdown");

    println!(
        "{}",
        json!({
            "benchmark": "code_mode_host_latency",
            "cold_create_session": summary(&mut cold_session),
            "cold_first_empty_cell": summary(&mut cold_first_cell),
            "warm_empty_cell": summary(&mut empty),
            "warm_ten_nested_tool_calls": summary(&mut nested),
            "warm_cpu_sort_200k": summary(&mut cpu),
            "warm_alloc_objects_500k": summary(&mut alloc),
            "host_memory": memory,
        })
    );
}
