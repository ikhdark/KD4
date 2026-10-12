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
        assert_eq!(result["selector_bindings"][3]["qualified_name"], "S::make");
        assert_eq!(result["selector_bindings"][3]["kind"], "fn");
        assert_eq!(result["selector_bindings"][2]["qualified_name"], "impl Action<u8> for S");
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

#[tokio::test]
async fn semnav_syntax_errors_survive_search_and_snapshot_recovery() {
    let source = "struct S;\nimpl S {\n    fn broken(&self) {\n        let broken_marker = ;\n    }\n    fn healthy(&self) {\n        let healthy_marker = 1;\n    }\n}\nimpl S ??? {\n    fn uncertain(&self) {\n        let header_marker = 1;\n    }\n}\nstruct Wrapper<T>(T);\n#[cfg(any())]\nimpl<T> Wrapper<T>\nwhere T: Clone,\n{\n    fn generic(&self) {\n        let generic_marker = self.0.clone();\n    }\n}\n";
    // Authored grammar errors, not compiler facts: broken lacks an expression;
    // uncertain inherits an invalid owner header. The sibling and generic
    // method have valid headers/bodies even though the file cannot compile.
    let cases = [
        ("broken", "broken_marker", 3, 5, true),
        ("healthy", "healthy_marker", 6, 8, false),
        ("uncertain", "header_marker", 11, 13, true),
        ("generic", "generic_marker", 20, 22, false),
    ];
    let mut selectors = Vec::new();
    let mut searches = Vec::new();
    for (name, marker, start, _, _) in cases {
        selectors.push(json!({"kind":"symbol", "name":name}));
        selectors.push(json!({"kind":"enclosing", "line":start + 1}));
        let search = json!({"kind":"search", "query":marker, "enclosing":true, "context_lines":0});
        selectors.push(search.clone());
        searches.push(search);
    }
    let ToolSpec::Function(spec) = ReadFileHandler.spec() else { panic!("function"); };
    let read_schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
    let ToolSpec::Function(spec) = ReadToolOutputHandler.spec() else { panic!("function"); };
    let recovery_schema = jsonschema::validator_for(&spec.output_schema.unwrap().to_value()).unwrap();
    for script in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("syntax.rs");
        std::fs::write(&path, source).unwrap();
        let mut call = invocation(&path, json!(selectors), false).await;
        if script { call.source = ToolCallSource::CodeMode { cell_id:"semnav-syntax".into(), parent_call_id:None,
            runtime_tool_call_id:"semnav-syntax".into(), nested_deadline:None, cancellation_cause:None }; }
        let current = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        read_schema.validate(&current).unwrap();
        assert_eq!(current["complete"], true, "parse uncertainty does not change exact-byte delivery");
        let bindings = current["selector_bindings"].as_array().unwrap();
        assert_eq!(bindings.len(), cases.len() * 2);
        for (index, (_, _, start, end, expected_error)) in cases.iter().enumerate() {
            for selector_index in [index * 3, index * 3 + 1] {
                let binding = bindings.iter().find(|binding|
                    binding["selector_index"].as_u64() == Some(selector_index as u64)).unwrap();
                assert_eq!(binding["resolved_selectors"], json!([{"kind":"lines", "start":start, "end":end}]));
                assert_eq!(binding["syntax_error"] == true, *expected_error);
                if !expected_error { assert!(binding.get("syntax_error").is_none()); }
            }
        }

        // Recovery must use the original bytes and stored parser warning, not
        // reparsing the now-healthy current file or silently clearing the flag.
        std::fs::write(&path, "fn replacement() {}\n").unwrap();
        call.tool_name = ToolName::plain("read_tool_output");
        call.payload = ToolPayload::Function { arguments:json!({"artifact_id":current["artifact_id"],
            "selectors":searches}).to_string() };
        let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        recovery_schema.validate(&recovered).unwrap();
        assert_eq!(recovered["canonical_sha256"], current["source_sha256"]);
        for output in [&current, &recovered] {
            assert_eq!(output["complete"], true);
            let evidence = semnav_evidence(output);
            for (_, marker, start, end, expected_error) in cases {
                let search = output["results"].as_array().unwrap().iter().find(|result|
                    result["selector"]["query"] == marker).unwrap();
                let item = &search["value"]["matches"][0]["enclosing_item"];
                assert_eq!((item["start_line"].as_u64(), item["end_line"].as_u64()),
                    (Some(start), Some(end)));
                assert_eq!(item["syntax_error"] == true, expected_error, "{marker}");
                if !expected_error { assert!(item.get("syntax_error").is_none()); }
                assert_eq!(search["value"]["enclosing_complete"], true);
                let expected = source.lines().skip(start as usize - 1)
                    .take((end - start + 1) as usize).collect::<Vec<_>>().join("\n") + "\n";
                assert!(evidence.iter().any(|(_, _, text)| text.contains(&expected)), "{marker}");
            }
            for (start, end, text) in evidence {
                assert_eq!(text.as_bytes(), &source.as_bytes()[start as usize..end as usize]);
            }
        }
    }
}

#[test]
fn semnav_nested_generics_python_and_duplicate_impls_stay_unambiguous() {
    for (path, source, name, expected) in [
        ("a.rs", "mod m { impl<T> Action<Vec<T>> for S<T> { fn run() {} } }", "m::S::run", "m::<S<T> as Action<Vec<T>>>::run"),
        ("a.rs", "impl Action<<T as Other>::Item> for S { fn run() {} }", "S::run", "<S as Action<<T as Other>::Item>>::run"),
        // Header spellings the caller cannot know before reading the file.
        ("a.rs", "impl<T> fmt::Display for S<T> { fn fmt() {} }", "impl Display for S", "impl fmt::Display for S<T>"),
        ("a.rs", "impl<T> fmt::Display for S<T> { fn fmt() {} }", "<S as Display>::fmt", "<S<T> as fmt::Display>::fmt"),
        ("a.rs", "impl<T> Stack<T> { fn push() {} }", "impl Stack", "impl Stack<T>"),
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
    // A wrong owner lists same-named items as diagnostics; an absent name lists none.
    let mut canonical = CanonicalToolResult::text("struct A;\nstruct B;\nimpl A { fn build() {} }\nfn free() {}\n");
    let (resolved, errors, _) = structure::resolve_batch("a.rs", &mut canonical, Some(
        ["B::build", "B::free", "B::absent"].into_iter().map(|name|
            serde_json::from_value(json!({"kind":"symbol","name":name})).unwrap()).collect()));
    assert!(resolved.unwrap().is_empty());
    assert_eq!(errors[0]["candidates"], json!([{"qualified_name":"A::build",
        "selector":{"kind":"lines","start":3,"end":3}}]));
    assert_eq!(errors[1]["candidates"][0]["qualified_name"], "free");
    assert_eq!(errors[2]["candidates"], json!([]));
}

#[tokio::test]
async fn semnav_module_paths_follow_inline_overrides_and_recover_original_outline() {
    // Independent rustc dep-info oracles distinguish crate-root / #[path]
    // inclusion from conventional modules. The first inline path replaces
    // either pending stem, but after an ordinary inline module both remain.
    for (file, source, expected) in [
        ("lib.rs", "#[path=\"chosen\"] mod inline { mod child; #[path=\"explicit.rs\"] mod named; }", &["chosen"][..]),
        ("parent.rs", "#[path=\"chosen\"] mod inline { mod child; #[path=\"explicit.rs\"] mod named; }", &["chosen"][..]),
        ("parent.rs", "mod outer { #[path=\"chosen\"] mod inline { mod child; #[path=\"explicit.rs\"] mod named; } }", &["outer/chosen", "parent/outer/chosen"][..]),
        ("lib.rs", "mod r#type { mod child; #[path=\"explicit.rs\"] mod named; }", &["type", "lib/type"][..]),
        ("parent.rs", "mod inline { mod child; #[path=\"explicit.rs\"] mod named; }", &["inline", "parent/inline"][..]),
    ] {
        for script in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(file);
            std::fs::write(&path, source).unwrap();
            let mut call = invocation(&path, json!([{"kind":"section","id":"outline"}]), false).await;
            if script { call.source = ToolCallSource::CodeMode {cell_id:"semnav-paths".into(), parent_call_id:None,
                runtime_tool_call_id:"semnav-paths".into(), nested_deadline:None, cancellation_cause:None}; }
            let result = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
            assert_eq!(result["complete"], true);
            let items = result["results"][0]["value"]["details"]["items"].as_array().unwrap();
            let child = items.iter().find(|item| item["name"] == "child").unwrap();
            let named = items.iter().find(|item| item["name"] == "named").unwrap();
            let bases = expected.iter().map(|expected| expected.split('/').fold(
                dir.path().to_path_buf(), |base, component| base.join(component))).collect::<Vec<_>>();
            let child_paths = bases.iter().flat_map(|base|
                [base.join("child.rs"), base.join("child").join("mod.rs")]).collect::<Vec<_>>();
            assert_eq!(child["linked_path_candidates"], json!(child_paths));
            if bases.len() == 1 {
                assert_eq!(named["linked_path"], json!(bases[0].join("explicit.rs")));
                assert!(named.get("linked_path_candidates").is_none());
            } else {
                assert!(named.get("linked_path").is_none());
                assert_eq!(named["linked_path_candidates"], json!(bases.iter()
                    .map(|base| base.join("explicit.rs")).collect::<Vec<_>>()));
            }
            // Paths are candidates, not filesystem reads or existence claims.
            assert!(bases.iter().all(|base| !base.exists()));
            std::fs::write(&path, "mod changed;").unwrap();
            call.payload = ToolPayload::Function {arguments:json!({"artifact_id":result["artifact_id"],
                "selectors":[{"kind":"section","id":"outline"}]}).to_string()};
            let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
            assert_eq!(recovered["canonical_sha256"], result["source_sha256"]);
            assert_eq!(recovered["results"][0]["value"], result["results"][0]["value"]);
        }
    }
}

#[tokio::test]
async fn semnav_parameterized_test_attributes_preserve_test_discovery() {
    // test-case 3.3.1 documents parameterized test instances; its macro renders
    // sync cases with #[::core::prelude::v1::test]. These are source templates,
    // not a claim that cfg/imports resolve or that generated cases executed.
    let source = r#"use test_case::test_case;
#[test_case::test_case(2; "second_open")]
#[test_case::test_case(3; "final_open")]
fn replacement(replace_on: usize) { fn helper() {} }
#[test_case(1)]
fn imported(value: usize) {}
#[test]
fn ordinary() {}
#[case(1)]
fn case_only() {}
#[inline]
fn production() {}
#[custom::test_case(1)]
fn other_macro() {}
#[cfg_attr(test, test_case::test_case(1))]
fn conditional() {}
#[test_case::test_case(1)]
struct NotFunction;
"#;
    for script in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("parameterized.rs");
        std::fs::write(&path, source).unwrap();
        let mut call = invocation(&path, json!([{"kind":"section","id":"outline"}]), false).await;
        if script { call.source = ToolCallSource::CodeMode {cell_id:"semnav-tests".into(), parent_call_id:None,
            runtime_tool_call_id:"semnav-tests".into(), nested_deadline:None, cancellation_cause:None}; }
        let result = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        assert_eq!(result["complete"], true);
        let items = result["results"][0]["value"]["details"]["items"].as_array().unwrap();
        for name in ["replacement", "imported", "ordinary"] {
            let item = items.iter().find(|item| item["name"] == name).unwrap();
            assert_eq!(item["is_test"], true, "{name}");
            assert_eq!(item["in_test_context"], true, "{name}");
        }
        for name in ["case_only", "production", "other_macro", "conditional", "NotFunction"] {
            let item = items.iter().find(|item| item["name"] == name).unwrap();
            assert_eq!(item["is_test"], false, "{name}");
            assert_eq!(item["in_test_context"], false, "{name}");
        }
        let helper = items.iter().find(|item| item["name"] == "helper").unwrap();
        assert_eq!(helper["is_test"], false);
        assert_eq!(helper["in_test_context"], true);
        assert!(!items.iter().any(|item| item["name"] == "second_open" || item["name"] == "final_open"),
            "outline must not pretend to expand generated test cases");

        std::fs::write(&path, "fn changed() {}").unwrap();
        call.payload = ToolPayload::Function {arguments:json!({"artifact_id":result["artifact_id"],
            "selectors":[{"kind":"section","id":"outline"}]}).to_string()};
        let recovered = ReadToolOutputHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        assert_eq!(recovered["canonical_sha256"], result["source_sha256"]);
        assert_eq!(recovered["results"][0]["value"], result["results"][0]["value"]);

        std::fs::write(&path, "#[test_case::test_case(]\nfn malformed() {}\n").unwrap();
        call.payload = ToolPayload::Function {arguments:json!({"path":path,
            "selectors":[{"kind":"section","id":"outline"}]}).to_string()};
        let malformed = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        assert_eq!(malformed["complete"], true);
        let items = malformed["results"][0]["value"]["details"]["items"].as_array().unwrap();
        assert!(items.iter().all(|item| item["is_test"] == false),
            "malformed attribute syntax is not positive test evidence");
    }
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
