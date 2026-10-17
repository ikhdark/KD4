//! Opt-in narrow native benchmarks. Run through the named code_mode_lib target.
use super::*;

#[test]
fn indexed_aliases_preserve_ambiguous_and_dotted_namespace_behavior() {
    let catalog = EnabledToolCatalog::new([
        ("a", Some("ns"), "read"), ("ns__read", Some("other"), "read"),
        ("dot", Some("ns.with.dot"), "read"), ("unicode", Some("命名"), "读取"),
        ("plain", None, "a.b"), ("empty", Some(""), "read"),
    ].into_iter().map(|(global, namespace, name)| EnabledToolMetadata {
        global_name:global.into(), tool_name:ToolName::new(namespace.map(str::to_owned), name),
        description:"".into(), kind:CodeModeToolKind::Function, default_timeout_ms:None,
    }).collect()).unwrap();
    for query in ["a", "ns__read", "ns.read", "other.read", "dot", "ns.with.dot.read",
        "ns.with.dot__read", "命名.读取", "命名__读取", "plain", "a.b", ".read", "__read", "missing"] {
        let matches = catalog.tools.iter().enumerate().filter(|(_, tool)| {
            tool.global_name == query || tool.tool_name.to_string() == query
                || query.split_once('.').is_some_and(|(ns, name)|
                    tool.tool_name.namespace.as_deref() == Some(ns) && tool.tool_name.name == name)
        }).map(|(i, _)| i).collect::<Vec<_>>();
        assert_eq!(catalog.resolve_requested_name(query),
            (matches.len() == 1).then(|| matches[0]), "{query}");
    }
}

#[tokio::test]
async fn callables_preserve_aliases_metadata_and_mutable_tools() {
    let definitions = (0..100).map(|i| codex_code_mode_protocol::ToolDefinition {
        name:format!("ns{i}__read"), tool_name:ToolName::new(Some(format!("ns{i}")), "read"),
        description:format!("description {i}").into(),kind:CodeModeToolKind::Function,
        input_schema:None,output_schema:None,default_timeout_ms:None,
    }).collect();
    let (event_tx, mut rx) = mpsc::unbounded_channel();
    let request = ExecuteRequest {state_path:None,tool_call_id:"alias-test".into(),enabled_tools:definitions,
        source:r#"
            if (Object.keys(tools).length !== 200) throw Error('enumeration changed');
            await notify('before tools');
            const canonical = tools.ns7__read;
            if (canonical !== tools.ns7.read || canonical !== resolve_tool('ns7.read')) throw Error('alias identity');
            if (canonical.name !== 'ns7__read' || !canonical.description.includes('description 7')) throw Error('metadata');
            if (JSON.parse(JSON.stringify(canonical)).name !== 'ns7__read') throw Error('serialization');
            tools.ns7__read = 'replacement';
            if (resolve_tool('ns7.read') !== canonical || tools.ns7.read !== canonical) throw Error('mutable tools changed resolver');
            await canonical({});
        "#.into(),yield_time_ms:None,max_output_tokens:None,default_tool_timeout_ms:None};
    let (tx, _termination) = spawn_runtime(HashMap::new(), request, 60_000, event_tx,
        Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)), None).await.unwrap();
    assert!(matches!(rx.recv().await.unwrap(), RuntimeEvent::Started));
    let RuntimeEvent::Notify {id:Some(id), ..} = rx.recv().await.unwrap() else {panic!("notification")};
    tx.send(RuntimeCommand::NotificationResponse {id}).unwrap();
    let RuntimeEvent::ToolCall {id, name, ..} = rx.recv().await.unwrap() else {panic!("tool call")};
    assert_eq!(name, ToolName::new(Some("ns7".into()), "read"));
    tx.send(RuntimeCommand::ToolResponse {id,result:JsonValue::Null}).unwrap();
    assert!(matches!(rx.recv().await.unwrap(), RuntimeEvent::Result {error_text:None,output_loss:None,..}));
    assert!(rx.recv().await.is_none());
}

pub(crate) fn report(name: &str, samples: &[f64]) {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let value = serde_json::json!({"scenario":name,"samples_ms":samples,"median_ms":sorted[sorted.len()/2]});
    eprintln!("{value}");
    if let Some(directory) = std::env::var_os("KD4_CRITICAL_PATH_REPORT_DIR") {
        use std::io::Write;
        let path = std::path::PathBuf::from(directory).join(format!("{name}.json"));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path).unwrap();
        writeln!(file, "{value}").unwrap();
    }
}

#[test]
#[ignore = "narrow timing probe"]
fn critical_path_catalog_lookup_benchmark() {
    let catalog = EnabledToolCatalog::new((0..1024).map(|i| EnabledToolMetadata {
        global_name:format!("tool_{i}"), tool_name:ToolName::new(Some(format!("ns{i}")), "read"),
        description:"description".into(), kind:CodeModeToolKind::Function, default_timeout_ms:None,
    }).collect()).unwrap();
    let names = (0..1024).map(|i| format!("ns{i}.read")).collect::<Vec<_>>();
    let samples = (0..7).map(|_| {
        let start = std::time::Instant::now();
        for _ in 0..4 { for (i, name) in names.iter().enumerate() {
            assert_eq!(catalog.resolve_requested_name(std::hint::black_box(name)), Some(i));
        } }
        start.elapsed().as_secs_f64()*1000.0
    }).collect::<Vec<_>>();
    report("catalog-lookup", &samples);
}

#[tokio::test]
#[ignore = "narrow timing probe"]
async fn critical_path_sparse_catalog_cell_benchmark() {
    let definitions: Arc<[codex_code_mode_protocol::ToolDefinition]> = (0..1024).map(|i| codex_code_mode_protocol::ToolDefinition {
        name:format!("ns{i}__read"), tool_name:ToolName::new(Some(format!("ns{i}")), "read"),
        description:"tool arguments and result schema ".repeat(50).into(),
        kind:CodeModeToolKind::Function, input_schema:None, output_schema:None, default_timeout_ms:None,
    }).collect();
    let catalog = Arc::new(EnabledToolCatalog::from_definitions(&definitions).unwrap());
    let mut samples = Vec::new();
    for round in 0..8 {
        let start = std::time::Instant::now();
        for _ in 0..10 {
            let (event_tx, mut event_rx) = mpsc::unbounded_channel();
            let request = ExecuteRequest {state_path:None,tool_call_id:"bench".into(),enabled_tools:Arc::clone(&definitions),
                source:"if (tools.ns1.read !== resolve_tool('ns1.read')) throw Error('alias identity');".into(),
                yield_time_ms:None,max_output_tokens:None,default_tool_timeout_ms:None};
            let (_tx, _termination) = spawn_runtime_with_catalog(HashMap::new(), request, Arc::clone(&catalog),
                60_000, event_tx, Arc::new(OutputAdmission::new(MAX_BUFFERED_OUTPUT_BYTES)), None).await.unwrap();
            let mut completed = false;
            while let Some(event) = event_rx.recv().await {
                match event {
                    RuntimeEvent::Started => {},
                    RuntimeEvent::Result {error_text:None,output_loss:None,..} => completed=true,
                    other => panic!("unexpected benchmark event: {other:?}"),
                }
            }
            assert!(completed);
        }
        if round > 0 { samples.push(start.elapsed().as_secs_f64()*1000.0); }
    }
    report("sparse-catalog-cells", &samples);
}
