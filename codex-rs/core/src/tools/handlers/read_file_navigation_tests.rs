// Included in read_file::tests to exercise the owning handler and its fixtures.
const SEMNAV_SOURCE: &str = "struct S;\ntrait Action<T> { fn run(&self); }\nimpl Action<u8> for S {\n    fn run(&self) {}\n}\nimpl S {\n    fn make() -> Self { S }\n}\nmod nested {\n    struct S;\n    impl S { fn make() -> Self { S } }\n}\nfn caller() { S::make(); }\n";

fn semnav_selectors() -> serde_json::Value {
    json!([
        {"kind":"symbol", "name":"Action"},
        {"kind":"symbol", "name":"impl S"},
        {"kind":"symbol", "name":"impl Action<u8> for S"},
        {"kind":"symbol", "name":"S::make"},
        {"kind":"search", "query":"S::make()", "enclosing":true, "context_lines":0}
    ])
}

fn semnav_evidence(output: &serde_json::Value) -> Vec<(u64, u64, String)> {
    let mut evidence = Vec::new();
    for result in output["results"].as_array().unwrap() {
        assert_eq!(result["status"], "ok");
        assert_eq!(result["complete"], true);
        let ranges = result["value"]["hydrated_ranges"].as_array();
        for part in std::iter::once(result).chain(ranges.into_iter().flatten()) {
            if let Some(text) = part["text"].as_str() {
                evidence.push((part["canonical_range"]["start"].as_u64().unwrap(),
                    part["canonical_range"]["end"].as_u64().unwrap(), text.to_string()));
            }
        }
    }
    evidence.sort();
    evidence
}

#[tokio::test]
async fn semnav_impls_qualified_names_and_generic_repairs_are_snapshot_bound() {
    let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
    let input = jsonschema::validator_for(&serde_json::to_value(&spec.parameters).unwrap()).unwrap();
    let output = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
    for script in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("navigation.rs");
        std::fs::write(&path, SEMNAV_SOURCE).unwrap();
        let mut call = invocation(&path, semnav_selectors(), false).await;
        if script { call.source = ToolCallSource::CodeMode { cell_id:"semnav".into(), parent_call_id:None,
            runtime_tool_call_id:"semnav".into(), nested_deadline:None, cancellation_cause:None }; }
        let ToolPayload::Function { arguments } = &call.payload else { panic!("function"); };
        input.validate(&serde_json::from_str::<serde_json::Value>(arguments).unwrap()).unwrap();
        let result = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        output.validate(&result).unwrap();
        assert_eq!(result["complete"], true);
        assert_eq!(result["selector_bindings"][1]["resolved_selectors"], json!([{"kind":"lines","start":6,"end":8}]));
        assert_eq!(result["selector_bindings"][2]["resolved_selectors"], json!([{"kind":"lines","start":3,"end":5}]));
        assert_eq!(result["selector_bindings"][3]["resolved_selectors"], json!([{"kind":"lines","start":7,"end":7}]));
        assert!(semnav_evidence(&result).iter().any(|(_, _, text)| text == "fn caller() { S::make(); }\n"));

        call.payload = ToolPayload::Function { arguments:json!({"path":path,"selectors":[
            {"kind":"symbol","name":"make"}, {"kind":"symbol","name":"S::run"},
            {"kind":"symbol","name":"<S as Action<u8>>::run"}, {"kind":"symbol","name":"S"}
        ]}).to_string() };
        let repaired = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        output.validate(&repaired).unwrap();
        assert_eq!(repaired["complete"], false, "diagnostic aliases must never execute");
        assert_eq!(repaired["selector_errors"].as_array().unwrap().len(), 3);
        assert_eq!(repaired["selector_errors"][0]["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(repaired["selector_errors"][1]["candidates"].as_array().unwrap().len(), 1);
        assert_eq!(repaired["selector_errors"][1]["candidates"][0]["qualified_name"], "<S as Action<u8>>::run");
        assert_eq!(repaired["selector_errors"][2]["candidates"].as_array().unwrap().len(), 2, "impls must not collide with type names");
        std::fs::write(&path, "replacement").unwrap();
        call.payload = ToolPayload::Function { arguments:json!({"artifact_id":repaired["artifact_id"],
            "selectors":[repaired["selector_errors"][1]["candidates"][0]["selector"]]}).to_string() };
        let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        assert_eq!(recovered["canonical_sha256"], repaired["source_sha256"]);
        assert_eq!(recovered["results"][0]["text"], "    fn run(&self) {}\n");
    }
}

#[test]
fn semnav_nested_generics_python_and_duplicate_impls_stay_unambiguous() {
    for (path, source, name, expected) in [
        ("a.rs", "mod m { impl<T> Action<Vec<T>> for S<T> { fn run() {} } }", "m::S::run", "m::<S<T> as Action<Vec<T>>>::run"),
        ("a.rs", "impl Action<<T as Other>::Item> for S { fn run() {} }", "S::run", "<S as Action<<T as Other>::Item>>::run"),
    ] {
        let mut canonical = CanonicalToolResult::text(source.to_string());
        let (resolved, errors, _) = structure::resolve_batch(path, &mut canonical, Some(vec![
            serde_json::from_value(json!({"kind":"symbol","name":name})).unwrap()]));
        assert!(resolved.unwrap().is_empty());
        assert_eq!(errors[0]["candidates"][0]["qualified_name"], expected);
    }
    let mut canonical = CanonicalToolResult::text("class A:\n def run(self): pass\ndef outer():\n class A:\n  def run(self): pass\n");
    let (resolved, errors, _) = structure::resolve_batch("a.py", &mut canonical, Some(vec![
        serde_json::from_value(json!({"kind":"symbol","name":"A.run"})).unwrap()]));
    assert!(errors.is_empty());
    assert_eq!(resolved.unwrap(), vec![ToolOutputSelector::Lines {start:2, end:2}]);
    let mut canonical = CanonicalToolResult::text("struct S;\nimpl S {}\nimpl S {}\n");
    let (_, errors, _) = structure::resolve_batch("a.rs", &mut canonical, Some(vec![
        serde_json::from_value(json!({"kind":"symbol","name":"impl S"})).unwrap()]));
    assert_eq!(errors[0]["candidates"].as_array().unwrap().len(), 2);
}

#[tokio::test]
#[ignore = "opt-in matched navigation wall-clock benchmark; no model latency simulation"]
#[expect(clippy::print_stdout, reason = "report measured navigation latency and call counts")]
async fn semnav_batched_definition_impl_and_caller_benchmark() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("benchmark.rs");
    let source = format!("{SEMNAV_SOURCE}{}", (0..1200).map(|i| format!("fn unrelated_{i}() {{}}\n")).collect::<String>());
    std::fs::write(&path, &source).unwrap();
    for script in [false, true] {
        let mut call = invocation(&path, semnav_selectors(), false).await;
        if script { call.source = ToolCallSource::CodeMode {cell_id:"semnav-bench".into(), parent_call_id:None,
            runtime_tool_call_id:"semnav-bench".into(), nested_deadline:None, cancellation_cause:None}; }
        let mut observations = Vec::new();
        for iteration in 0..7 {
            let mut pair = Vec::new();
            for batched in if iteration % 2 == 0 { [false, true] } else { [true, false] } {
                let start = std::time::Instant::now();
                call.payload = ToolPayload::Function {arguments:json!({"path":path,"selectors":if batched {
                    semnav_selectors()
                } else { json!([{"kind":"section","id":"outline"}]) }}).to_string()};
                let first = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
                let mut bytes = serde_json::to_vec(&first).unwrap().len();
                let selected = if batched { first } else {
                    let items = first["results"][0]["value"]["details"]["items"].as_array().unwrap();
                    let mut selectors = ["Action", "impl S", "impl Action<u8> for S", "S::make"].map(|name| {
                        let item = items.iter().find(|item| item["qualified_name"] == name).unwrap();
                        json!({"kind":"lines","start":item["start_line"],"end":item["end_line"]})
                    }).to_vec();
                    selectors.push(semnav_selectors()[4].clone());
                    call.payload = ToolPayload::Function {arguments:json!({"artifact_id":first["artifact_id"],
                        "selectors":selectors}).to_string()};
                    let selected = ReadToolOutputHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
                    assert_eq!(selected["canonical_sha256"], first["source_sha256"]);
                    bytes += serde_json::to_vec(&selected).unwrap().len();
                    selected
                };
                let elapsed = start.elapsed().as_micros();
                assert_eq!(selected["complete"], true);
                pair.push(semnav_evidence(&selected));
                observations.push(json!({"iteration":iteration,"batched":batched,"wall_us":elapsed,
                    "tool_calls":if batched {1} else {2},"output_bytes":bytes}));
            }
            assert_eq!(pair[0], pair[1], "exact evidence and byte ranges must agree");
        }
        println!("SEMANTIC_NAV {}", json!({"script":script,"source_bytes":source.len(),
            "iterations":7,"accuracy":"all paired exact ranges and bytes equal","model_calls":0,
            "observations":observations}));
    }
}
