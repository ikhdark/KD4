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
    start_with_tool(source, ToolDefinition {
        name: "sample_tool".to_string(),
        tool_name: ToolName::plain("sample_tool"),
        kind: CodeModeToolKind::Function,
        description: "".into(),
        input_schema: None,
        output_schema: None,
        default_timeout_ms: None,
    }).await
}

async fn start_with_tool(source: &str, tool: ToolDefinition) -> (std_mpsc::Sender<RuntimeCommand>, RuntimeTerminationHandle, mpsc::UnboundedReceiver<RuntimeEvent>) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let request = ExecuteRequest {
        state_path: None,
        tool_call_id: "regression".to_string(),
        enabled_tools: vec![tool].into(),
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
async fn dependency_graph_scheduling_contracts() {
    let (_tx, _termination, mut rx) = start(include_str!("dependency_graph_tests.js")).await;
    assert_eq!(text(next(&mut rx).await), "dependency graph scenarios passed");
    let RuntimeEvent::Result { error_text, output_loss, .. } = next(&mut rx).await else {
        panic!("graph result");
    };
    assert_eq!(error_text, None);
    assert_eq!(output_loss, None);
    closed(&mut rx).await;
}

#[tokio::test]
async fn resolver_diagnostics_are_opt_in_non_callable_and_actionable() {
    let (_tx, _termination, mut rx) = start(r#"
        if (resolve_tool('missing') !== undefined) throw Error('compatibility');
        const invalid = resolve_tool(null, {diagnostic:true});
        const miss = resolve_tool('sample', {diagnostic:true});
        if (invalid.status !== 'invalid_name' || miss.status !== 'not_found' ||
            miss.candidates.join() !== 'sample_tool' || typeof miss === 'function') throw Error('diagnostic');
        if (typeof resolve_tool(miss.candidates[0], {diagnostic:true}) !== 'function') throw Error('resolution');
        text('resolution diagnostics passed');
    "#).await;
    assert_eq!(text(next(&mut rx).await), "resolution diagnostics passed");
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text: None, .. }));
    closed(&mut rx).await;
}

#[tokio::test]
async fn mechanical_orchestration_preserves_evidence_and_stops_for_decisions() {
    let (_tx, _termination, mut rx) = start(include_str!("orchestration_tests.js")).await;
    assert_eq!(text(next(&mut rx).await), "orchestration scenarios passed");
    let RuntimeEvent::Result { error_text, output_loss, .. } = next(&mut rx).await else {
        panic!("orchestration result");
    };
    assert_eq!(error_text, None);
    assert_eq!(output_loss, None);
    closed(&mut rx).await;
}

#[tokio::test]
async fn selected_rows_preserve_parent_mutations_getters_proxies_and_to_json() {
    let source = r#"
        const r = await tools.read_file({});
        text(r.results.map(row => row));
        let reads = 0;
        text([r.results[0], {get value() { reads++; r.path='changed'; return r.results[1]; }}]);
        text(reads); r.path='source';
        text([r.results[0], {toJSON() { r.path='changed'; return r.results[1]; }}]);
        r.path='source';
        const proxy = new Proxy({}, {get(_target,key) { if(key==='toJSON') r.path='changed'; }});
        text([r.results[0], proxy, r.results[1]]);
        r.path='changed'; text(r.results);
    "#;
    let (tx, _termination, mut rx) = start_with_tool(source, ToolDefinition {
        name: "read_file".into(), tool_name: ToolName::plain("read_file"),
        kind: CodeModeToolKind::Function, description: "".into(), input_schema: None,
        output_schema: None, default_timeout_ms: None,
    }).await;
    let RuntimeEvent::ToolCall { id, .. } = next(&mut rx).await else { panic!("read call") };
    let raw = json!({"path":"source","artifact_id":"artifact","canonical_bytes":13,
        "canonical_sha256":"hash","source_sha256":"hash","complete":true,
        "results":[
            {"selector":{"kind":"lines","start":1,"end":1},"status":"ok","complete":true,"text":"first\n"},
            {"selector":{"kind":"lines","start":2,"end":2},"status":"ok","complete":true,"text":"second\n"}
        ]});
    tx.send(RuntimeCommand::ToolResponse { id, result: raw.clone() }).unwrap();
    let batch = text(next(&mut rx).await);
    assert_eq!(batch.matches("[source ").count(), 2);
    let getter = text(next(&mut rx).await);
    assert_eq!(getter.matches("[source ").count(), 1);
    assert!(getter.contains("second\\n"));
    assert_eq!(text(next(&mut rx).await), "1");
    for _ in 0..2 {
        let dynamic = text(next(&mut rx).await);
        assert_eq!(dynamic.matches("[source ").count(), 1);
        assert!(dynamic.contains("second\\n"));
    }
    assert_eq!(serde_json::from_str::<serde_json::Value>(&text(next(&mut rx).await)).unwrap(), raw["results"]);
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text: None, .. }));
    closed(&mut rx).await;
}

#[test]
fn stored_serialization_is_shared_and_replaced_with_its_value() {
    let key = "quote\"日本語";
    let value = json!({"text": "λ\r\n", "nested": [true, null, 42]});
    let stored = StoredValue::new(key, value.clone());
    assert_eq!(stored.bytes, stored_value_entry_bytes(key, &value));
    let snapshot = stored.clone();
    assert!(Arc::ptr_eq(stored.serialized.as_ref().unwrap(), snapshot.serialized.as_ref().unwrap()));
    assert_eq!(serde_json::from_str::<JsonValue>(stored.serialized.as_deref().unwrap().get()).unwrap(), value);
    let changed = StoredValue::new(key, json!({"text":"changed"}));
    assert_ne!(changed.serialized.as_deref().unwrap().get(), stored.serialized.as_deref().unwrap().get());
    assert_eq!(snapshot.value.as_ref(), &value);
}

#[tokio::test]
async fn tool_owned_deadlines_and_projection_recovery_reach_the_live_runtime() {
    for (name, default_timeout, arguments, options, expected) in [
        (ToolName::plain("shell_command"), Some(315_000), "{}", "undefined", 315_000),
        (ToolName::plain("shell_command"), Some(315_000), "{timeout_ms:600000}", "undefined", 615_000),
        (ToolName::plain("shell_command"), Some(315_000), "{timeout_ms:600000}", "{timeout_ms:1234}", 1234),
        (ToolName::plain("shell_command"), Some(315_000), "{timeout_ms:3600000}", "undefined", 0),
        (ToolName::plain("ordinary"), None, "{timeout_ms:600000}", "undefined", 60_000),
        (ToolName::namespaced("mcp", "shell_command"), Some(75_000), "{timeout_ms:600000}", "undefined", 75_000),
        (ToolName::plain("owned"), Some(0), "{}", "undefined", 0),
    ] {
        let mut definition = codex_code_mode_protocol::augment_tool_definition(ToolDefinition {
            name: "sample_tool".into(), tool_name: name, kind: CodeModeToolKind::Function,
            description: "".into(), default_timeout_ms: default_timeout,
            input_schema: Some(json!({"type":"object", "properties":{
                "payload":{"type":"string", "description":format!("{}critical suffix", "x".repeat(150_000))}
            }})), output_schema: None,
        });
        // Same schema removal as the core construction path: resolution must
        // still provide the authoritative fallback in this cell, not next turn.
        definition.input_schema = None;
        let source = format!(r#"
            const tool = resolve_tool('sample_tool');
            const raw = tool.description.split('```json\n')[1].split('\n```')[0];
            text(JSON.parse(raw).properties.payload.description.endsWith('critical suffix'));
            await tool({arguments}, {options});
        "#);
        let (tx, _termination, mut rx) = start_with_tool(&source, definition).await;
        assert_eq!(text(next(&mut rx).await), "true");
        let RuntimeEvent::ToolCall { id, timeout_ms, .. } = next(&mut rx).await else { panic!("tool call"); };
        assert_eq!(timeout_ms, expected);
        tx.send(RuntimeCommand::ToolResponse { id, result: json!(null) }).unwrap();
        assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text: None, .. }));
        closed(&mut rx).await;
    }
}

#[tokio::test]
async fn uncaught_helpers_preserve_settled_evidence() {
    let (_tx, _termination, mut rx) = start(r#"
        await run_graph([
          {id:"failed", run:() => { throw new Error("original failure"); }, accept:() => true},
          {id:"skipped", deps:["failed"], run:() => { throw new Error("must not run"); }, accept:() => true},
          ...Array.from({length:8}, (_, i) => ({id:`sibling-${i}`, run:() => ({artifact_id:`artifact-${i}`}), accept:() => true}))
        ]);
    "#).await;
    let RuntimeEvent::Result { error_text: Some(error), .. } = next(&mut rx).await else { panic!("helper failure"); };
    let (_, json) = error.split_once("\nHelper results:\n").expect("settled graph evidence");
    let results: JsonValue = serde_json::from_str(json).unwrap();
    assert_eq!(results["failed"]["reason"]["message"], "original failure");
    assert_eq!(results["skipped"]["status"], "skipped");
    for i in 0..8 {
        assert_eq!(results[format!("sibling-{i}")]["value"]["artifact_id"], format!("artifact-{i}"));
    }
    closed(&mut rx).await;
}

#[tokio::test]
async fn uncaught_oversized_helpers_keep_every_settled_status_and_recovery_handle() {
    let (_tx, _termination, mut rx) = start(r#"
        await run_graph([
          {id:'failed', run:()=>{throw new Error('failure')}, accept:()=>true},
          ...Array.from({length:100}, (_, i)=>({id:`sibling-${i}`,
            run:()=>({output:'x'.repeat(100000), artifact_id:`artifact-${i}`}), accept:()=>true}))
        ]);
    "#).await;
    let error = text(next(&mut rx).await);
    let RuntimeEvent::Result { error_text:Some(summary), output_loss, .. } = next(&mut rx).await else { panic!("helper failure"); };
    assert!(summary.len() <= MAX_ERROR_TEXT_BYTES);
    assert_eq!(output_loss, None);
    assert!(error.len() < 8 * 1024 * 1024);
    let (_, json) = error.split_once("\nHelper results:\n").unwrap();
    let evidence: JsonValue = serde_json::from_str(json).unwrap();
    assert_eq!(evidence["bounded_helper_evidence"], true);
    assert_eq!(evidence["omitted_entries"], 0);
    let entries = evidence["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 101);
    assert_eq!(entries[0]["value"]["status"], "rejected");
    for i in 0..100 {
        assert_eq!(entries[i+1]["value"]["status"], "fulfilled");
        assert_eq!(entries[i+1]["value"]["value"]["artifact_id"], format!("artifact-{i}"));
    }
    closed(&mut rx).await;
}

#[tokio::test]
async fn unsafe_integers_round_trip_through_nested_tools_and_storage() {
    let raw = json!({"values":[9_007_199_254_740_991_u64, 9_007_199_254_740_993_u64,
        u64::MAX, i64::MIN], "nested":{"id":-9_007_199_254_740_993_i64}});
    let tool = ToolDefinition {
        name:"sample_tool".into(), tool_name:ToolName::plain("sample_tool"),
        kind:CodeModeToolKind::Function, description:"".into(), default_timeout_ms:None,
        input_schema:Some(json!({"type":"object","properties":{
            "values":{"type":"array","items":{"type":"integer"}},
            "nested":{"type":"object","properties":{"id":{"type":"integer"}}}
        }})), output_schema:None,
    };
    let (tx, _termination, mut rx) = start_with_tool(r#"
        const r = await tools.sample_tool({});
        text([typeof r.values[0], typeof r.values[1], r.values[1], r.nested.id]);
        store('exact', r);
        await tools.sample_tool(load('exact'));
        try { store('overflow', 18446744073709551616n); }
        catch(error) { text(error.message.includes('exact JSON integer transport range')); }
    "#, tool).await;
    let RuntimeEvent::ToolCall { id, .. } = next(&mut rx).await else { panic!("initial call"); };
    tx.send(RuntimeCommand::ToolResponse { id, result:raw.clone() }).unwrap();
    let printed: JsonValue = serde_json::from_str(&text(next(&mut rx).await)).unwrap();
    assert_eq!(printed, json!(["number","bigint",{"$bigint":"9007199254740993"},{"$bigint":"-9007199254740993"}]));
    let RuntimeEvent::ToolCall { id, input:Some(input), .. } = next(&mut rx).await else { panic!("exact round trip"); };
    assert_eq!(input, raw);
    tx.send(RuntimeCommand::ToolResponse { id, result:json!(null) }).unwrap();
    assert_eq!(text(next(&mut rx).await), "true");
    let RuntimeEvent::Result { error_text, stored_value_writes, .. } = next(&mut rx).await else { panic!("result"); };
    assert_eq!(error_text, None);
    assert_eq!(*stored_value_writes["exact"].value, raw);
    assert!(!stored_value_writes.contains_key("overflow"));
    closed(&mut rx).await;
}

#[tokio::test]
async fn printed_settled_errors_are_bounded_without_mutating_script_evidence() {
    let (_tx, _termination, mut rx) = start(r#"
        const cause = new TypeError('middle diagnostic');
        const failure = new Error('x'.repeat(100000), {cause});
        cause.cause = failure;
        failure.evidence = {artifact_id:'recover-me'};
        const results = await run_graph([
            {id:'bad', run:()=>{throw failure}, accept:()=>true},
            {id:'good', run:()=>({important:'successful sibling'}), accept:()=>true}
        ]).catch(error=>error.results);
        text(results);
        text(JSON.stringify(results));
        console.log(await Promise.allSettled([Promise.reject(failure), Promise.resolve('kept')]));
        text(results.bad.reason === failure && results.bad.reason.message.length === 100000);
        let deep = new Error('leaf');
        for (let i=0;i<100;i++) deep = new Error('nested',{cause:deep});
        text({reason:deep, sibling:'kept'});
        const bulky = new Error('bounded evidence');
        bulky.evidence = new Array(1000000);
        bulky.evidence[999999] = 'last original position';
        text({reason:bulky, sibling:'kept'});
        bulky.evidence = {['x'.repeat(100000)]: 'too large a key'};
        text({reason:bulky, sibling:'kept'});
    "#).await;
    let printed = text(next(&mut rx).await);
    assert!(printed.len() < 6000);
    let value: JsonValue = serde_json::from_str(&printed).unwrap();
    assert_eq!(value["bad"]["reason"]["name"], "Error");
    assert_eq!(value["bad"]["reason"]["cause"]["message"], "middle diagnostic");
    assert_eq!(value["bad"]["reason"]["cause"]["cause"], "[circular error evidence]");
    assert_eq!(value["bad"]["reason"]["evidence"]["artifact_id"], "recover-me");
    assert_eq!(value["good"]["value"]["important"], "successful sibling");
    assert_eq!(text(next(&mut rx).await), printed);
    let batch: JsonValue = serde_json::from_str(&text(next(&mut rx).await)).unwrap();
    assert_eq!(batch[0]["reason"]["cause"]["message"], "middle diagnostic");
    assert_eq!(batch[1]["value"], "kept");
    assert_eq!(text(next(&mut rx).await), "true");
    assert!(text(next(&mut rx).await).contains("[error details truncated]"));
    for _ in 0..2 {
        let printed = text(next(&mut rx).await);
        assert!(printed.len() < 1000);
        let value: JsonValue = serde_json::from_str(&printed).unwrap();
        assert_eq!(value["reason"]["message"], "bounded evidence");
        assert_eq!(value["sibling"], "kept");
        if value["reason"]["evidence"]["original_length"] == 1_000_000 {
            assert_eq!(value["reason"]["evidence"]["entries"]["999999"], "last original position");
        }
    }
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text:None, .. }));
    closed(&mut rx).await;
}

#[tokio::test]
async fn stored_loads_are_independent_and_replacement_invalidates_serialization() {
    let (_tx, _termination, mut rx) = start(r#"
        store("result", {nested: {value: "original"}});
        const first = load("result");
        first.nested.value = "local mutation";
        text(load("result").nested.value);
        store("result", {nested: {value: "replacement"}});
        text(load("result").nested.value);
    "#).await;
    assert_eq!(text(next(&mut rx).await), "original");
    assert_eq!(text(next(&mut rx).await), "replacement");
    let RuntimeEvent::Result { error_text, .. } = next(&mut rx).await else { panic!("result"); };
    assert_eq!(error_text, None);
    closed(&mut rx).await;
}

#[tokio::test]
async fn dependency_graph_cancellation_stops_pending_and_dependent_dispatch() {
    let (tx, termination, mut rx) = start(r#"
        await run_graph([
          {id: "a", resources: {write: ["repo"]}, run: () => tools.sample_tool({id: "a"}), accept: () => true},
          {id: "b", run: () => tools.sample_tool({id: "b"}), accept: () => true},
          {id: "pending", resources: {write: ["repo"]}, run: () => tools.sample_tool({id: "pending"}), accept: () => true},
          {id: "dependent", deps: ["a"], run: () => tools.sample_tool({id: "dependent"}), accept: () => true},
        ], {concurrency: 2});
        text("must not complete");
    "#).await;
    let mut started = Vec::new();
    for _ in 0..2 {
        let RuntimeEvent::ToolCall { input, .. } = next(&mut rx).await else {
            panic!("expected admitted graph tool");
        };
        started.push(input.unwrap()["id"].as_str().unwrap().to_string());
    }
    assert_eq!(started, ["a", "b"]);
    // Synchronize with the idle event loop, then use the real owner cancellation
    // path. No graph-specific cancellation token can bypass nested cleanup.
    inspect(&tx).await;
    assert!(termination.terminate_execution());
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            assert!(matches!(event, RuntimeEvent::Result { .. }));
        }
    }).await.expect("cancelled graph runtime must close");
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
    assert!(baseline.stored_payload_shared, "store must share its immutable payload");
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
    let payload = stored_value_writes["payload"].value.as_str().unwrap();
    assert_eq!(payload, "x".repeat(1_000_000));
    assert_eq!(payload.as_ptr() as usize, baseline.stored_payload_address, "finalization must move, not clone");
    closed(&mut rx).await;
}

#[tokio::test]
async fn cancellation_exits_cpu_idle_and_unclaimed_startup_threads() {
    for source in ["text('entered'); while (true) {}", "text('entered'); await new Promise(resolve => setTimeout(resolve, 600_000));"] {
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
        const calls = Array.from({{length: {MAX_OUTSTANDING_CALLBACKS_PER_CELL}}}, () => tools.sample_tool({{}}));
        let conversions = 0;
        const value = {{toJSON() {{ conversions++; return 'large'; }}}};
        const failures = [];
        for (const call of [() => tools.sample_tool(value), () => notify(value)]) {{
            try {{ await call(); failures.push(false); }} catch (e) {{ failures.push(e instanceof Error); }}
        }}
        text({{conversions, failures}});
        await Promise.all(calls);
    "#);
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
        ("await new Promise(() => {});", Some("unresolved top-level promise")),
        ("await new Promise(resolve => setTimeout(resolve, 1)); await new Promise(() => {});", Some("unresolved top-level promise")),
        ("await new Promise(resolve => setTimeout(resolve, 1));", None),
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
async fn abandoned_tools_are_detected_before_held_notifications() {
    let (_tx, _termination, mut rx) = start("tools.sample_tool({}); notify('held');").await;
    assert!(matches!(next(&mut rx).await, RuntimeEvent::ToolCall { .. }));
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Notify { .. }));
    assert!(matches!(next(&mut rx).await, RuntimeEvent::Result { error_text: Some(error), .. }
        if error.contains("unawaited tool")));
    closed(&mut rx).await;
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
