//! Full-turn regression coverage for production tool-output compaction and recovery.
use super::assert_eq;
use super::*;
use serde_json::json;
use std::sync::Mutex;

const BEFORE: &str = "before λ 日本語\n";
const AFTER: &str = "after λ 日本語\n";
const ROW: &str = "ROW_0500: exact retained evidence for operation 500";

fn output_text(item: &Value) -> String {
    match &item["output"] {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        other => panic!("not a tool output: {other}"),
    }
}

fn output<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "custom_tool_call_output" && item["call_id"] == id)
        .unwrap_or_else(|| panic!("missing output {id}: {body}"))
}

fn payload(body: &Value, id: &str) -> Value {
    output_text(output(body, id))
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v.get("kind").is_some())
        .unwrap_or_else(|| panic!("missing payload {id}: {}", output_text(output(body, id))))
}

fn notice(item: &Value) -> Option<(String, Value)> {
    if item["role"] != "developer" {
        return None;
    }
    let text = item["content"][0]["text"].as_str()?;
    if !text.starts_with("<workspace_evidence_invalidation>") {
        return None;
    }
    let (prefix, rest) = text.split_once("\n{")?;
    let record = format!(
        "{{{}",
        rest.strip_suffix("\n</workspace_evidence_invalidation>")?
    );
    Some((prefix.to_owned(), serde_json::from_str(&record).unwrap()))
}

fn freshness(body: &Value) -> Vec<Value> {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(notice)
        .flat_map(|(_, value)| {
            value
                .get("notices")
                .or_else(|| value.get("results"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_else(|| vec![value])
        })
        .collect()
}

fn read_text(body: &Value, id: &str) -> String {
    let file = payload(body, id);
    assert_eq!(file["result"]["complete"], true);
    assert_eq!(file["result"]["file_complete"], true);
    let raw = output_text(output(body, id));
    let start = raw.find("{\"kind\":").expect("file payload envelope");
    projected_native_text(&raw[start..], &file["result"]["results"][0])
}

fn next_action(body: &Value, step: usize) -> Value {
    let script = match step {
        0 => {
            assert!(body["tools"].to_string().contains("read_file"));
            "const r=await tools.read_file({path:'contract.txt'}); if(r.canonical_sha256!==r.source_sha256 || r.delivered_selection_complete!==r.complete || r.artifact_id!==null) throw Error('raw read_file shape changed'); const raw=JSON.stringify(r); text({kind:'file',result:r}); if(JSON.stringify(r)!==raw) throw Error('printing mutated result');".to_owned()
        }
        1 => {
            assert_eq!(read_text(body, "step-0"), BEFORE);
            let file = payload(body, "step-0")["result"].clone();
            for key in ["canonical_sha256", "delivered_selection_complete", "artifact_id"] {
                assert!(file.get(key).is_none(), "redundant file field: {key}");
            }
            assert!(file["source_sha256"].is_string());
            "text({kind:'search',result:await tools.tool_search({query:'mcp__bench__mirror',limit:1})});".to_owned()
        }
        2 => {
            assert!(
                !payload(body, "step-1")["result"]["tools"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            "text({kind:'search',result:await tools.tool_search({query:'mcp__bench__mirror',limit:1})});".to_owned()
        }
        3 => {
            let repeated = payload(body, "step-2");
            let schema = if let Some(previous) = repeated["result"]["previous_call_id"].as_str() {
                payload(body, previous)["result"].clone()
            } else {
                repeated["result"].clone()
            };
            assert_eq!(schema, payload(body, "step-1")["result"]);
            assert!(schema.to_string().contains("mirror"));
            let name = codex_tools::code_mode_name_for_tool_name(&codex_tools::ToolName::namespaced("mcp__bench", "mirror"));
            format!("const r=await tools[{name:?}]({{message:'verified'}}); if(r.content.length!==2) throw Error('raw MCP shape changed'); text({{kind:'mcp',result:r}});")
        }
        4 => {
            let mcp = payload(body, "step-3");
            assert_eq!(mcp["result"]["structuredContent"]["message"], "verified");
            assert_eq!(mcp["result"]["isError"], false);
            assert_eq!(mcp["result"]["content"].as_array().unwrap().len(), 1);
            assert!(
                mcp["result"]["content"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|v| v["text"] == "caption survives")
            );
            let command = r#"python -u -c "from pathlib import Path; import time; Path('producer-count.txt').open('a').write('run\n'); print(Path('evidence.txt').read_text(), end='', flush=True); time.sleep(2)""#;
            // Keep the recovery fixture bounded independently of changing defaults.
            format!(
                "const r=await tools.exec_command({{cmd:{command:?},yield_time_ms:1000,max_output_tokens:4000}}); if(!r.raw_output_artifact_id) throw Error('missing artifact'); store('command-result',r); text(r.output);"
            )
        }
        5 => {
            let text = output_text(output(body, "step-4"));
            assert!(text.contains("ROW_0000"));
            assert!(!text.contains(ROW), "fixture must exercise actual recovery");
            "const initial=load('command-result'); let r=initial; while(r.session_id && !r.process_exited) r=await tools.write_stdin({session_id:r.session_id,incarnation:r.session_capabilities.incarnation,chars:''}); const recovered=await tools.read_tool_output({artifact_id:initial.raw_output_artifact_id,selectors:[{kind:'lines',start:501,end:501}]}); text({kind:'recovered',result:recovered,exited:r.process_exited,exit_code:r.exit_code});".to_owned()
        }
        6 => {
            let recovered = payload(body, "step-5");
            assert_eq!(recovered["exited"], true);
            assert_eq!(recovered["exit_code"], 0);
            assert_eq!(recovered["result"]["complete"], true);
            assert!(output_text(output(body, "step-5")).contains(ROW));
            "text({kind:'file',result:await tools.read_file({path:'contract.txt'})});".to_owned()
        }
        7 => {
            assert_eq!(read_text(body, "step-6"), BEFORE);
            let patch = "*** Begin Patch\n*** Update File: contract.txt\n@@\n-before λ 日本語\n+after λ 日本語\n*** End Patch\n";
            format!("text(await tools.apply_patch({patch:?}));")
        }
        8 => {
            let notices = freshness(body);
            for id in ["step-0", "step-6"] {
                assert!(
                    notices
                        .iter()
                        .any(|v| v["call_id"] == id && v["stale_workspace_evidence"] == true),
                    "missing invalidation {id}: {notices:?}"
                );
                assert_eq!(
                    read_text(body, id),
                    BEFORE,
                    "do not rewrite historical evidence"
                );
            }
            "text({kind:'file',result:await tools.read_file({path:'contract.txt'})});".to_owned()
        }
        9 => {
            let current = read_text(body, "step-8");
            assert_eq!(current, AFTER);
            let mcp = payload(body, "step-3");
            let message = mcp["result"]["structuredContent"]["message"]
                .as_str()
                .unwrap();
            // The final action depends on data consumed after projection, not on
            // an unconditional scripted success response.
            let patch = format!(
                "*** Begin Patch\n*** Add File: result.txt\n+{}\n+{ROW}\n+{message}\n*** End Patch\n",
                current.trim_end()
            );
            format!("text(await tools.apply_patch({patch:?}));")
        }
        10 => {
            assert!(output_text(output(body, "step-9")).contains("result.txt"));
            return ev_assistant_message(
                "verified",
                "Verified current contract, retained evidence, and MCP result.",
            );
        }
        _ => panic!("unexpected extra model request {step}"),
    };
    ev_custom_tool_call(&format!("step-{step}"), "exec", &script)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_compact_outputs_complete_turn_non_regression() -> Result<()> {
    require_network!();
    let server = responses::start_mock_server().await;
    let mcp_calls = Arc::new(Mutex::new(Vec::<Value>::new()));
    let calls = Arc::clone(&mcp_calls);
    Mock::given(method("POST")).and(path("/mcp")).respond_with(move |request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let result = match body["method"].as_str().unwrap() {
            "initialize" => json!({"protocolVersion":body["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"token-cache-fixture","version":"1"}}),
            "notifications/initialized" => return ResponseTemplate::new(202),
            "tools/list" => json!({"tools":[{"name":"mirror","description":"Echo a verification message with JSON text and structured content.","inputSchema":{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]},"annotations":{"readOnlyHint":true}}]}),
            "tools/call" => {
                calls.lock().unwrap().push(body["params"].clone());
                assert_eq!(body["params"]["name"],"mirror");
                let data=json!({"message":body["params"]["arguments"]["message"],"rows":(0..20).map(|i|json!({"id":i,"status":"verified"})).collect::<Vec<_>>()});
                json!({"content":[{"type":"text","text":"caption survives"},{"type":"text","text":data.to_string()}],"structuredContent":data,"isError":false})
            }
            method => panic!("unexpected MCP method {method}"),
        };
        ResponseTemplate::new(200).set_body_json(json!({"jsonrpc":"2.0","id":body["id"],"result":result}))
    }).mount(&server).await;
    let url = format!("{}/mcp", server.uri());
    let mut builder = test_codex().with_config(move |config| {
        config.features.enable(Feature::CodeMode).unwrap();
        config.completed_tool_history_projection = true;
        let mut catalog = bundled_models_response().unwrap();
        let selected = catalog
            .models
            .iter_mut()
            .find(|m| Some(m.slug.as_str()) == config.model.as_deref())
            .unwrap();
        selected.supports_search_tool = true;
        config.model_catalog = Some(catalog);
        let mut servers = config.mcp_servers.get().clone();
        servers.insert(
            "bench".to_owned(),
            serde_json::from_value(json!({"url":url,"startup_timeout_sec":10})).unwrap(),
        );
        config.mcp_servers.set(servers).unwrap();
    });
    let test = builder.build(&server).await?;
    wait_for_mcp_server(&test.codex, "bench").await?;
    fs::write(test.cwd_path().join("contract.txt"), BEFORE)?;
    fs::write(test.cwd_path().join("untouched.txt"), "must not change\n")?;
    let evidence = (0..1400)
        .map(|i| format!("ROW_{i:04}: exact retained evidence for operation {i}\n"))
        .collect::<String>();
    fs::write(test.cwd_path().join("evidence.txt"), &evidence)?;
    let run = Arc::new(Mutex::new(Vec::<Value>::new()));
    let state = Arc::clone(&run);
    let responder = Mock::given(method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses$"))
        .respond_with(move |request: &wiremock::Request| {
            let bytes = if request
                .headers
                .get("content-encoding")
                .is_some_and(|v| v == "zstd")
            {
                zstd::stream::decode_all(std::io::Cursor::new(&request.body)).unwrap()
            } else {
                request.body.clone()
            };
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            let mut state = state.lock().unwrap();
            let action = next_action(&body, state.len());
            let response = sse(vec![
                ev_response_created("response"),
                action,
                ev_completed("response"),
            ]);
            state.push(body);
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(response)
        });
    responder.expect(11).mount(&server).await;
    let completion=test.submit_turn_and_capture_completion("Read contract.txt, discover the mirror tool twice, verify its result and ROW_0500 from the command, update the contract, then write result.txt using current evidence. Do not modify untouched.txt.").await?;
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("Verified current contract, retained evidence, and MCP result.")
    );
    assert_eq!(
        fs::read_to_string(test.cwd_path().join("result.txt"))?,
        format!("{AFTER}{ROW}\nverified\n")
    );
    assert_eq!(
        fs::read_to_string(test.cwd_path().join("producer-count.txt"))?.replace("\r\n", "\n"),
        "run\n",
        "no replay of producer for recovery"
    );
    assert_eq!(
        fs::read_to_string(test.cwd_path().join("evidence.txt"))?,
        evidence
    );
    assert_eq!(
        fs::read_to_string(test.cwd_path().join("untouched.txt"))?,
        "must not change\n"
    );
    assert_eq!(
        mcp_calls.lock().unwrap().len(),
        1,
        "schema dedup must not duplicate MCP execution"
    );
    let state = run.lock().unwrap();
    assert_eq!(state.len(), 11);
    for body in state.iter() {
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .starts_with(state[0]["input"].as_array().unwrap()),
            "initial input prefix must remain stable"
        );
    }
    Ok(())
}
