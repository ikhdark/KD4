use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::ToolDefinition;
use codex_protocol::ToolName;
use serde_json::json;
use tokio::sync::mpsc;

use super::*;

async fn start(source: &str) -> (std_mpsc::Sender<RuntimeCommand>, RuntimeTerminationHandle, mpsc::UnboundedReceiver<RuntimeEvent>) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let request = ExecuteRequest {
        tool_call_id: "regression".to_string(),
        enabled_tools: vec![ToolDefinition {
            name: "sample_tool".to_string(),
            tool_name: ToolName::plain("sample_tool"),
            kind: CodeModeToolKind::Function,
            description: String::new(),
            input_schema: None,
            output_schema: None,
            default_timeout_ms: None,
        }],
        source: source.to_string(),
        yield_time_ms: Some(1),
        max_output_tokens: None,
        default_tool_timeout_ms: None,
    };
    let (tx, termination) = spawn_runtime(HashMap::new(), request, 60_000, event_tx,
        Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)), None).await.unwrap();
    assert!(matches!(next(&mut event_rx).await, RuntimeEvent::Started));
    (tx, termination, event_rx)
}

async fn next(rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) -> RuntimeEvent {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("runtime event deadline").expect("runtime event")
}

async fn closed(rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>) {
    assert!(tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().is_none());
}

async fn inspect(tx: &std_mpsc::Sender<RuntimeCommand>) -> RuntimeInspection {
    let (response, rx) = tokio::sync::oneshot::channel();
    tx.send(RuntimeCommand::InspectForTest { response }).unwrap();
    tokio::time::timeout(Duration::from_secs(5), rx).await.unwrap().unwrap()
}

fn text(event: RuntimeEvent) -> String {
    match event {
        RuntimeEvent::ContentItem { item: FunctionCallOutputContentItem::InputText { text }, .. } => text,
        event => panic!("expected text, got {event:?}"),
    }
}

#[tokio::test]
async fn pending_checks_do_not_collect_writes_and_response_heap_does_not_accumulate() {
    let (tx, _termination, mut rx) = start(r#"
        store("payload", "x".repeat(1_000_000));
        for (let i = 0; i < 65; i++) await tools.sample_tool({});
    "#).await;
    let RuntimeEvent::ToolCall { mut id, .. } = next(&mut rx).await else { panic!("tool call"); };
    let baseline = inspect(&tx).await;
    assert_ne!(baseline.stored_payload_address, 0);
    assert_eq!(baseline.completion_collections, 0);
    for _ in 0..64 {
        tx.send(RuntimeCommand::ToolResponse { id, result: json!({"data": "r".repeat(1_000_000)}) }).unwrap();
        let RuntimeEvent::ToolCall { id: next_id, .. } = next(&mut rx).await else { panic!("next tool call"); };
        id = next_id;
    }
    let after = inspect(&tx).await;
    assert_eq!(after.completion_collections, 0);
    assert_eq!(after.stored_payload_address, baseline.stored_payload_address);
    assert!(after.heap_bytes < baseline.heap_bytes + 4 * 1024 * 1024,
        "discarded responses survived GC: {baseline:?} -> {after:?}");
    tx.send(RuntimeCommand::ToolResponse { id, result: json!(null) }).unwrap();
    let RuntimeEvent::Result { stored_value_writes, error_text, output_loss } = next(&mut rx).await else { panic!("result"); };
    assert_eq!(error_text, None);
    assert_eq!(output_loss, None);
    assert_eq!(stored_value_writes.len(), 1);
    let payload = stored_value_writes["payload"].as_str().unwrap();
    assert_eq!(payload, "x".repeat(1_000_000));
    assert_eq!(payload.as_ptr() as usize, baseline.stored_payload_address, "finalization must move, not clone");
    closed(&mut rx).await;
}

#[tokio::test]
async fn cancellation_exits_cpu_idle_and_unclaimed_startup_threads() {
    for source in ["text('entered'); while (true) {}", "text('entered'); await new Promise(() => {});"] {
        for unclaimed in [false, true] {
            let (release_tx, release_rx) = std_mpsc::channel();
            release_tx.send(()).unwrap();
            let gate = Arc::new(StartupTestGate {
                entered: tokio::sync::Notify::new(),
                release: std::sync::Mutex::new(release_rx),
                exited: tokio::sync::Notify::new(),
            });
            let (tx, termination, mut rx) = STARTUP_TEST_GATE.scope(Arc::clone(&gate), start(source)).await;
            assert_eq!(text(next(&mut rx).await), "entered");
            if source.contains("new Promise") { inspect(&tx).await; }
            if unclaimed {
                let (startup_tx, startup_rx) = tokio::sync::oneshot::channel();
                assert!(startup_tx.send(StartupIsolateHandle(Some(termination))).is_ok());
                drop(startup_rx);
            } else {
                assert!(termination.terminate_execution());
            }
            tokio::time::timeout(Duration::from_secs(5), gate.exited.notified()).await.expect("runtime thread must exit");
            while let Some(event) = rx.recv().await {
                assert!(matches!(event, RuntimeEvent::Result { .. }));
            }
        }
    }
}

#[tokio::test]
async fn disconnected_host_stops_new_tool_and_notification_requests() {
    for callback in ["tools.sample_tool({})", "notify('message')"] {
        let (release_tx, release_rx) = std_mpsc::channel();
        release_tx.send(()).unwrap();
        let gate = Arc::new(StartupTestGate {
            entered: tokio::sync::Notify::new(), release: std::sync::Mutex::new(release_rx), exited: tokio::sync::Notify::new(),
        });
        let source = format!("await tools.sample_tool({{}}); {callback}; await new Promise(() => {{}});");
        let (tx, _termination, mut rx) = STARTUP_TEST_GATE.scope(Arc::clone(&gate), start(&source)).await;
        let RuntimeEvent::ToolCall { id, .. } = next(&mut rx).await else { panic!("tool call"); };
        drop(rx);
        tx.send(RuntimeCommand::ToolResponse { id, result: json!(null) }).unwrap();
        tokio::time::timeout(Duration::from_secs(5), gate.exited.notified()).await.expect("disconnected host must not strand runtime");
    }
}

#[tokio::test]
async fn payload_limits_reject_without_queueing_large_values() {
    let source = format!(r#"
        const checks = [];
        for (const run of [
            () => text("x".repeat({payload} + 1)),
            () => tools.sample_tool({{data: "x".repeat({payload})}}),
            () => notify("x".repeat({notification} + 1)),
        ]) {{
            try {{ await run(); checks.push(false); }} catch (e) {{ checks.push(e instanceof TypeError && e.message.includes('limit')); }}
        }}
        text(checks);
        await tools.sample_tool({{}});
    "#, payload = value::MAX_PAYLOAD_BYTES, notification = value::MAX_NOTIFICATION_BYTES);
    let (tx, _termination, mut rx) = start(&source).await;
    assert_eq!(text(next(&mut rx).await), "[true,true,true]");
    let RuntimeEvent::ToolCall { id, input, .. } = next(&mut rx).await else { panic!("admitted barrier call"); };
    assert_eq!(input, Some(json!({})));
    assert!(inspect(&tx).await.rust_conversion_bytes < 1024, "rejected payloads must not be copied into Rust");
    tx.send(RuntimeCommand::ToolResponse { id, result: json!(null) }).unwrap();
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text: None, output_loss: None, .. }));
    closed(&mut rx).await;
}

#[tokio::test]
async fn full_callback_capacity_does_not_run_json_conversion() {
    let source = format!(r#"
        const calls = Array.from({{length: {limit}}}, () => tools.sample_tool({{}}));
        let conversions = 0;
        const value = {{toJSON() {{ conversions++; return 'large'; }}}};
        const failures = [];
        for (const call of [() => tools.sample_tool(value), () => notify(value)]) {{
            try {{ await call(); failures.push(false); }} catch (e) {{ failures.push(e instanceof Error); }}
        }}
        text({{conversions, failures}});
        await Promise.all(calls);
    "#, limit = MAX_OUTSTANDING_CALLBACKS_PER_CELL);
    let (tx, _termination, mut rx) = start(&source).await;
    let mut ids = Vec::new();
    for _ in 0..MAX_OUTSTANDING_CALLBACKS_PER_CELL {
        let RuntimeEvent::ToolCall { id, input, .. } = next(&mut rx).await else { panic!("accepted call"); };
        assert_eq!(input, Some(json!({})));
        ids.push(id);
    }
    assert_eq!(text(next(&mut rx).await), r#"{"conversions":0,"failures":[true,true]}"#);
    for id in ids { tx.send(RuntimeCommand::ToolResponse { id, result: json!(null) }).unwrap(); }
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text: None, .. }));
    closed(&mut rx).await;
}

#[tokio::test]
async fn standard_errors_and_console_diagnostics_survive_recovery() {
    let (tx, _termination, mut rx) = start(r#"
        try { setTimeout(null, 0); } catch (e) { text([e instanceof TypeError, e.message]); }
        try { await tools.sample_tool({}); } catch (e) { text([e instanceof Error, e.message]); }
        try { await notify('message'); } catch (e) { text([e instanceof Error, e.message]); }
        const circular = {}; circular.self = circular;
        console.error(new Error('diagnostic'), circular);
        text('recovered');
    "#).await;
    assert_eq!(text(next(&mut rx).await), r#"[true,"setTimeout expects a function callback"]"#);
    let RuntimeEvent::ToolCall { id, .. } = next(&mut rx).await else { panic!("tool call"); };
    tx.send(RuntimeCommand::ToolError { id, error_text: "tool failed".to_string() }).unwrap();
    assert_eq!(text(next(&mut rx).await), r#"[true,"tool failed"]"#);
    let RuntimeEvent::Notify { id: Some(id), .. } = next(&mut rx).await else { panic!("notification"); };
    tx.send(RuntimeCommand::NotificationError { id, error_text: "notification failed".to_string() }).unwrap();
    assert_eq!(text(next(&mut rx).await), r#"[true,"notification failed"]"#);
    let diagnostic = text(next(&mut rx).await);
    assert!(diagnostic.contains("Error: diagnostic"), "{diagnostic}");
    assert!(diagnostic.contains("exec_main.mjs"), "{diagnostic}");
    assert!(diagnostic.contains("[unserializable object]"), "{diagnostic}");
    assert_eq!(text(next(&mut rx).await), "recovered");
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text: None, .. }));
    closed(&mut rx).await;
}

#[tokio::test]
async fn async_rejections_are_reported_unless_a_handler_is_attached() {
    for (source, expected_error) in [
        ("await Promise.resolve();", None),
        ("await Promise.reject(new Error('awaited failure'));", Some("awaited failure")),
        ("const p = Promise.reject(new Error('handled')); await Promise.resolve(); p.catch(() => {});", None),
        ("Promise.reject(new Error('unhandled failure')); await new Promise(() => {});", Some("Unhandled promise rejection: Error: unhandled failure")),
        ("setTimeout(async () => { throw new Error('callback failed'); }, 0); await new Promise(resolve => setTimeout(resolve, 20));", Some("Unhandled promise rejection: Error: callback failed")),
    ] {
        let (_tx, _termination, mut rx) = start(source).await;
        let RuntimeEvent::Result { error_text, .. } = next(&mut rx).await else { panic!("terminal result for {source}"); };
        match expected_error {
            Some(expected) => assert!(error_text.as_deref().is_some_and(|error| error.contains(expected)), "{source}: {error_text:?}"),
            None => assert_eq!(error_text, None, "{source}"),
        }
        closed(&mut rx).await;
    }
}

#[tokio::test]
async fn images_require_a_supported_base64_payload() {
    let (_tx, _termination, mut rx) = start(r#"
        const invalid = ['data:', 'data:text/plain;base64,YQ==', 'data:image/png;base64,!!!!', 'data:image/png;base64,YR==', 'data:image/png;utf8,x'];
        text(invalid.map(uri => { try { image(uri); return false; } catch (e) { return e instanceof TypeError; } }));
        image('DATA:image/png;base64,AAAA');
    "#).await;
    assert_eq!(text(next(&mut rx).await), "[true,true,true,true,true]");
    assert!(matches!(next(&mut rx).await, RuntimeEvent::ContentItem { item: FunctionCallOutputContentItem::InputImage { image_url, .. }, .. } if image_url == "DATA:image/png;base64,AAAA"));
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text: None, .. }));
    closed(&mut rx).await;
}
