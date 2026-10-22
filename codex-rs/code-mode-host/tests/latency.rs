//! Host-backed Code Mode latency benchmark (validation T8). Ignored by default:
//! `cargo test -p codex-code-mode-host --test latency -- --ignored --nocapture`
//! prints one JSON line with per-scenario millisecond statistics and, on
//! Windows, the warm host's memory after the allocation-heavy cells.
#![allow(clippy::expect_used)]

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::ExecuteRequest;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ProcessOwnedCodeModeSessionProvider;
use codex_code_mode::RuntimeResponse;
use codex_code_mode::ToolDefinition;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode_protocol::NestedCancellation;
use codex_protocol::ToolName;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct EchoDelegate {
    calls: Mutex<Vec<CodeModeNestedToolCall>>,
}

impl CodeModeSessionDelegate for EchoDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        _cancellation: NestedCancellation,
    ) -> ToolInvocationFuture<'a> {
        let value = invocation
            .input
            .as_ref()
            .and_then(|input| input["index"].as_u64())
            .map(|index| index * 2 + 1);
        // Ten calls are expected. Retain one excess receipt to reject extra
        // work without allowing a broken workload to grow an unbounded trace.
        let mut calls = self.calls.lock().expect("calls lock");
        if calls.len() < 11 {
            calls.push(invocation);
        }
        Box::pin(async move { Ok(json!({ "value": value })) })
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
let checksum = 0;
for (let index = 0; index < 10; index += 1) {
  const result = await tools.echo({ index });
  if (result.value !== index * 2 + 1) throw new Error("wrong echo response");
  checksum += result.value;
}
text(String(checksum));
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

fn validate_cell(
    response: &RuntimeResponse,
    expected_cell_id: &CellId,
    expected: &str,
    calls: &[CodeModeNestedToolCall],
    nested: bool,
) -> bool {
    let RuntimeResponse::Result {
        cell_id,
        content_items,
        error_text: None,
        output_loss: None,
    } = response
    else {
        return false;
    };
    if cell_id != expected_cell_id
        || content_items
            != &vec![FunctionCallOutputContentItem::InputText {
                text: expected.to_string(),
            }]
        || calls.len() != if nested { 10 } else { 0 }
    {
        return false;
    }
    let mut ids = HashSet::new();
    calls.iter().enumerate().all(|(index, call)| {
        call.cell_id == *cell_id
            && call.parent_tool_call_id.as_deref() == Some("latency")
            && call.tool_name == ToolName::plain("echo")
            && call.tool_kind == CodeModeToolKind::Function
            && call.input == Some(json!({ "index": index }))
            && !call.runtime_tool_call_id.is_empty()
            && ids.insert(&call.runtime_tool_call_id)
    })
}

async fn timed_cell(
    session: &Arc<dyn CodeModeSession>,
    delegate: &EchoDelegate,
    source: &str,
) -> Duration {
    assert!(delegate.calls.lock().expect("calls lock").is_empty());
    let started_at = Instant::now();
    let started = session.execute(request(source)).await.expect("start cell");
    let cell_id = started.cell_id.clone();
    let response = started.initial_response().await.expect("cell response");
    let elapsed = started_at.elapsed();
    let calls = std::mem::take(&mut *delegate.calls.lock().expect("calls lock"));
    let expected = match source {
        EMPTY_CELL => "ok",
        NESTED_CELL => "100",
        CPU_CELL => "400002",
        ALLOC_CELL => "5388890",
        _ => panic!("unknown benchmark workload"),
    };
    assert!(
        validate_cell(&response, &cell_id, expected, &calls, source == NESTED_CELL),
        "benchmark workload failed: {response:?}; calls: {calls:?}"
    );
    elapsed
}

/// Missing workload/output/receipts must never qualify as a latency sample.
#[test]
fn workload_oracle_rejects_missing_work() {
    let cell_id = CellId::new("oracle".to_string());
    let response = RuntimeResponse::Result {
        cell_id: cell_id.clone(),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "100".to_string(),
        }],
        error_text: None,
        output_loss: None,
    };
    let calls: Vec<_> = (0..10)
        .map(|index| CodeModeNestedToolCall {
            cell_id: cell_id.clone(),
            parent_tool_call_id: Some("latency".to_string()),
            runtime_tool_call_id: format!("call-{index}"),
            buffered_output_bytes: 0,
            tool_name: ToolName::plain("echo"),
            tool_kind: CodeModeToolKind::Function,
            input: Some(json!({ "index": index })),
            nested_deadline: None,
        })
        .collect();
    assert!(validate_cell(&response, &cell_id, "100", &calls, true));
    assert!(!validate_cell(
        &response,
        &CellId::new("other".to_string()),
        "100",
        &calls,
        true
    ));
    for count in [0, 9, 11] {
        let mut wrong = calls.clone();
        wrong.resize(count, calls[0].clone());
        assert!(!validate_cell(&response, &cell_id, "100", &wrong, true));
    }
    for index in 0..10 {
        let mut wrong = calls.clone();
        wrong[index].input = Some(json!({ "index": 99 }));
        assert!(!validate_cell(&response, &cell_id, "100", &wrong, true));
    }
    let mut wrong = calls.clone();
    wrong[9] = calls[0].clone();
    assert!(!validate_cell(&response, &cell_id, "100", &wrong, true));
    wrong = calls.clone();
    wrong[0].tool_name = ToolName::plain("other");
    assert!(!validate_cell(&response, &cell_id, "100", &wrong, true));
    for (expected, actual) in [("ok", ""), ("400002", "400001"), ("5388890", "0")] {
        let bad = RuntimeResponse::Result {
            cell_id: cell_id.clone(),
            content_items: if actual.is_empty() {
                vec![]
            } else {
                vec![FunctionCallOutputContentItem::InputText {
                    text: actual.to_string(),
                }]
            },
            error_text: None,
            output_loss: None,
        };
        assert!(!validate_cell(&bad, &cell_id, expected, &[], false));
    }
    let mut errored = response.clone();
    if let RuntimeResponse::Result { error_text, .. } = &mut errored {
        *error_text = Some("error".to_string());
    }
    let mut lost = response;
    if let RuntimeResponse::Result { output_loss, .. } = &mut lost {
        *output_loss = Some(codex_code_mode_protocol::OutputLoss::default());
    }
    for bad in [
        errored,
        lost,
        RuntimeResponse::Yielded {
            cell_id: cell_id.clone(),
            content_items: vec![],
        },
    ] {
        assert!(!validate_cell(&bad, &cell_id, "100", &calls, true));
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
        let delegate = Arc::new(EchoDelegate::default());
        let started_at = Instant::now();
        let session = provider
            .create_session(delegate.clone())
            .await
            .expect("create session");
        cold_session.push(started_at.elapsed());
        cold_first_cell.push(timed_cell(&session, &delegate, EMPTY_CELL).await);
        session.shutdown().await.expect("shutdown");
    }

    let provider = ProcessOwnedCodeModeSessionProvider::with_host_program(program);
    let delegate = Arc::new(EchoDelegate::default());
    let session = provider
        .create_session(delegate.clone())
        .await
        .expect("create session");
    for _ in 0..5 {
        timed_cell(&session, &delegate, EMPTY_CELL).await;
    }
    let mut empty = Vec::new();
    for _ in 0..50 {
        empty.push(timed_cell(&session, &delegate, EMPTY_CELL).await);
    }
    let mut nested = Vec::new();
    for _ in 0..30 {
        nested.push(timed_cell(&session, &delegate, NESTED_CELL).await);
    }
    let mut cpu = Vec::new();
    for _ in 0..15 {
        cpu.push(timed_cell(&session, &delegate, CPU_CELL).await);
    }
    let mut alloc = Vec::new();
    for _ in 0..5 {
        alloc.push(timed_cell(&session, &delegate, ALLOC_CELL).await);
    }
    let memory = host_memory();
    session.shutdown().await.expect("shutdown");

    println!(
        "{}",
        json!({
            "benchmark": "code_mode_host_latency",
            "workload_version": 2,
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
