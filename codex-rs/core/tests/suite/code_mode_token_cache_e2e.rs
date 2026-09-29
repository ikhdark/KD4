//! Online provider-boundary A/B, not a replay of captured requests.
//! #17/#18 now run in production, including the adapter-off non-regression test.
//! Remaining candidates are applied before the deterministic provider acts.
use super::assert_eq;
use super::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::Mutex;

#[path = "code_mode_token_cache_live.rs"]
mod live;

const BEFORE: &str = "before λ 日本語\n";
const AFTER: &str = "after λ 日本語\n";
const ROW: &str = "ROW_0500: exact retained evidence for operation 500";

fn tokens(value: &Value) -> usize {
    codex_utils_output_truncation::model_token_count(&value.to_string())
}

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

fn replace_output_lines(item: &mut Value, mut transform: impl FnMut(&str) -> String) {
    fn replace(text: &mut Value, transform: &mut impl FnMut(&str) -> String) {
        if let Some(raw) = text.as_str() {
            *text = Value::String(
                raw.split('\n')
                    .map(transform)
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
    }
    match &mut item["output"] {
        Value::String(_) => replace(&mut item["output"], &mut transform),
        Value::Array(items) => {
            for part in items {
                replace(&mut part["text"], &mut transform);
            }
        }
        other => panic!("unexpected tool output {other}"),
    }
}

// Stateless projection of the visible request: losing prior history cannot leave
// a dangling search receipt. Existing schemas/results outside these exact shapes
// remain untouched. It is intentionally not installed in production transport.
fn project(raw: &Value, enabled: bool) -> Value {
    if !enabled || !raw["input"].is_array() {
        return raw.clone();
    }
    let mut body = raw.clone();
    if let Some(tools) = body.get_mut("tools") {
        compact_tools(tools);
    }
    for item in body["input"].as_array_mut().unwrap() {
        if item["type"] == "additional_tools" {
            compact_tools(&mut item["tools"]);
        }
    }
    let mut schemas = BTreeMap::<String, String>::new();
    for item in body["input"].as_array_mut().unwrap() {
        if item["type"] != "custom_tool_call_output" {
            continue;
        }
        let id = item["call_id"].as_str().unwrap().to_owned();
        replace_output_lines(item, |line| {
            let Ok(mut value) = serde_json::from_str::<Value>(line) else {
                return line.to_owned();
            };
            match value["kind"].as_str() {
                Some("search") => {
                    let key = value["result"].to_string();
                    if let Some(previous) = schemas.get(&key) {
                        value["result"] =
                            json!({"already_available":true,"previous_call_id":previous});
                    } else {
                        schemas.insert(key, id.clone());
                    }
                }
                _ => return line.to_owned(),
            }
            value.to_string()
        });
    }
    // Batch only adjacent invalidation messages; never reorder history or grow a
    // single notice. Preserve all per-result freshness/recovery metadata exactly.
    let input = body["input"].as_array_mut().unwrap();
    let mut index = 0;
    while index < input.len() {
        let Some(first) = notice(&input[index]) else {
            index += 1;
            continue;
        };
        if first.1.get("notices").is_some() {
            index += 1;
            continue;
        }
        let mut end = index + 1;
        let mut records = vec![first.1];
        while end < input.len() {
            let Some((prefix, record)) = notice(&input[end]) else {
                break;
            };
            if prefix != first.0 || record.get("notices").is_some() {
                break;
            }
            records.push(record);
            end += 1;
        }
        if records.len() > 1 {
            input[index]["content"][0]["text"] = json!(format!(
                "{}\n{}\n</workspace_evidence_invalidation>",
                first.0,
                json!({"results":records})
            ));
            input.drain(index + 1..end);
        }
        index += 1;
    }
    body
}

fn compact_tools(value: &mut Value) {
    let Some(tools) = value.as_array_mut() else {
        return;
    };
    // #2 applies only to mixed mode. Never remove the only declaration of a
    // nested tool from a code-mode-only catalog.
    if !tools.iter().any(|tool| tool["name"] == "read_file") {
        return;
    }
    for tool in tools {
        if tool["name"] == "exec"
            && let Some(description) = tool["description"].as_str()
            && let Some((prefix, _)) = description.split_once("\n\nEager nested tool contracts:")
        {
            tool["description"] = json!(format!(
                "{prefix}\n\nDirect contracts are advertised separately; resolve missing nested contracts with resolve_tool(name)."
            ));
        }
    }
}

fn tool_schemas(body: &Value) -> Vec<Value> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            body["input"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| v["type"] == "additional_tools")
                .flat_map(|v| v["tools"].as_array().into_iter().flatten()),
        )
        .cloned()
        .collect()
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
    file["result"]["results"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[derive(Default)]
struct Run {
    raw: Vec<Value>,
    visible: Vec<Value>,
    generated: Vec<Value>,
}

fn next_action(body: &Value, step: usize, candidate: bool) -> Value {
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
            "const r=await tools.mcp__bench__mirror({message:'verified'}); if(r.content.length!==2) throw Error('raw MCP shape changed'); text({kind:'mcp',result:r});".to_owned()
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
            let budget = if candidate {
                ",max_output_tokens:2500"
            } else {
                // Keep the recovery fixture bounded independently of changing defaults.
                ",max_output_tokens:4000"
            };
            format!(
                "const r=await tools.exec_command({{cmd:{command:?},yield_time_ms:1000{budget}}}); if(!r.raw_output_artifact_id) throw Error('missing artifact'); store('command-result',r); text(r.output);"
            )
        }
        5 => {
            let text = output_text(output(body, "step-4"));
            assert!(text.contains("ROW_0000"));
            assert!(!text.contains(ROW), "fixture must exercise actual recovery");
            "const initial=load('command-result'); let r=initial; while(r.session_id && !r.process_exited) r=await tools.write_stdin({session_id:r.session_id,chars:''}); const recovered=await tools.read_tool_output({artifact_id:initial.raw_output_artifact_id,selectors:[{kind:'lines',start:501,end:501}]}); text({kind:'recovered',result:recovered,exited:r.process_exited,exit_code:r.exit_code});".to_owned()
        }
        6 => {
            let recovered = payload(body, "step-5");
            assert_eq!(recovered["exited"], true);
            assert_eq!(recovered["exit_code"], 0);
            assert!(recovered["result"].to_string().contains(ROW));
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

async fn one_turn(candidate: bool, live: Option<Arc<live::LiveContext>>) -> Result<Value> {
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
    let model = live
        .as_ref()
        .map_or("gpt-5.4", |live| live.model.as_str())
        .to_owned();
    let catalog = live.as_ref().map(|live| live.catalog.clone());
    let configured_model = model.clone();
    let live_mode = live.is_some();
    let live_effort = live.as_ref().map(|live| live.effort.clone());
    let mut builder = test_codex().with_model(&model).with_config(move |config| {
        config.features.enable(Feature::CodeMode).unwrap();
        config.completed_tool_history_projection = true;
        let mut catalog = catalog.unwrap_or_else(|| bundled_models_response().unwrap());
        let selected = catalog
            .models
            .iter_mut()
            .find(|m| m.slug == configured_model)
            .unwrap();
        selected.supports_search_tool = true;
        if live_mode {
            selected.tool_mode = Some(codex_protocol::openai_models::ToolMode::CodeMode);
        }
        if let Some(effort) = live_effort {
            config.model_reasoning_effort = Some(serde_json::from_value(json!(effort)).unwrap());
        }
        config.model_catalog = Some(catalog);
        let mut servers = config.mcp_servers.get().clone();
        servers.insert(
            "bench".to_owned(),
            serde_json::from_value(json!({"url":url,"startup_timeout_sec":10})).unwrap(),
        );
        config.mcp_servers.set(servers).unwrap();
    });
    if live.is_some() {
        // Only the local adapter sees this dummy identity. It authenticates the
        // upstream request itself; credentials never enter the fixture home.
        builder = builder.with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing());
    }
    let test = builder.build(&server).await?;
    wait_for_mcp_server(&test.codex, "bench").await?;
    fs::write(test.cwd_path().join("contract.txt"), BEFORE)?;
    fs::write(test.cwd_path().join("untouched.txt"), "must not change\n")?;
    let evidence = (0..1400)
        .map(|i| format!("ROW_{i:04}: exact retained evidence for operation {i}\n"))
        .collect::<String>();
    fs::write(test.cwd_path().join("evidence.txt"), &evidence)?;
    if live.is_some() {
        fs::write(
            test.cwd_path().join("producer.py"),
            "from pathlib import Path\nimport time\nPath('producer-count.txt').open('a').write('run\\n')\nprint(Path('evidence.txt').read_text(), end='', flush=True)\ntime.sleep(2)\n",
        )?;
    }
    let run = Arc::new(Mutex::new(Run::default()));
    let state = Arc::clone(&run);
    let live_forward = live.clone();
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
            let raw: Value = serde_json::from_slice(&bytes).unwrap();
            let visible = project(&raw, candidate);
            if let Some(live) = &live_forward {
                return live.forward(raw, visible, candidate);
            }
            let mut state = state.lock().unwrap();
            let action = next_action(&visible, state.raw.len(), candidate);
            let response = sse(vec![
                ev_response_created("response"),
                action.clone(),
                ev_completed("response"),
            ]);
            state.raw.push(raw);
            state.visible.push(visible);
            state.generated.push(action);
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(response)
        });
    if live.is_some() {
        responder.mount(&server).await;
    } else {
        responder.expect(11).mount(&server).await;
    }
    if let Some(live) = live {
        return live
            .finish_turn(&test, &mcp_calls, &evidence, candidate)
            .await;
    }
    let start = Instant::now();
    let completion=test.submit_turn_and_capture_completion("Read contract.txt, discover the mirror tool twice, verify its result and ROW_0500 from the command, update the contract, then write result.txt using current evidence. Do not modify untouched.txt.").await?;
    let elapsed = start.elapsed();
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
    assert_eq!(state.raw.len(), 11);
    for body in &state.visible {
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .starts_with(state.visible[0]["input"].as_array().unwrap()),
            "initial input prefix must remain stable"
        );
    }
    let notice_messages = |body: &Value| {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| notice(item).is_some())
            .cloned()
            .collect::<Vec<_>>()
    };
    let raw_notices = notice_messages(&state.raw[8]);
    let visible_notices = notice_messages(&state.visible[8]);
    if candidate {
        // File/MCP compaction is already active without the candidate adapter.
        for (index, kind) in [(3, "search")] {
            let id = format!("step-{}", index - 1);
            assert!(
                tokens(output(&state.visible[index], &id)) < tokens(output(&state.raw[index], &id)),
                "{kind} candidate did not reach provider"
            );
        }
        assert!(
            tokens(&state.visible[0]["tools"]) < tokens(&state.raw[0]["tools"]),
            "duplicate contracts not removed"
        );
        assert_eq!(
            freshness(&state.raw[8]),
            freshness(&state.visible[8]),
            "batch must preserve exact stale records"
        );
        assert_eq!(raw_notices.len(), 1, "production must batch invalidations");
        assert!(freshness(&state.raw[8]).len() >= 2);
        assert_eq!(
            visible_notices, raw_notices,
            "the adapter must preserve already-batched production notices"
        );
        let single_notice = json!({"tools":[],"input":[raw_notices[0].clone()]});
        assert_eq!(
            project(&single_notice, true),
            single_notice,
            "a single notice must not grow"
        );
        // Dropping visible search history must restore the full schema, not emit
        // a receipt referencing a result absent from the model's current input.
        let mut lost = state.raw[3].clone();
        lost["input"]
            .as_array_mut()
            .unwrap()
            .retain(|item| item["call_id"] != "step-1");
        assert!(payload(&project(&lost, true), "step-2")["result"]["tools"].is_array());
    }
    let input_tokens = state.visible.iter().map(tokens).sum::<usize>();
    let output_tokens = state.generated.iter().map(tokens).sum::<usize>();
    let record = json!({"candidate":candidate,"model_requests":state.raw.len(),"input_tokens":input_tokens,"scripted_output_tokens":output_tokens,"total_tokens":input_tokens+output_tokens,"complete_turn_ms":elapsed.as_millis(),"command_output_tokens":tokens(output(&state.visible[5],"step-4")),"task_success":true,"recovery_calls":1,"mcp_calls":1,"raw_notice_messages":raw_notices.len(),"visible_notice_messages":visible_notices.len(),"per_request_input_tokens":state.visible.iter().map(tokens).collect::<Vec<_>>(),"scope":"online mock-provider boundary experiment; not billed tokens or live-model inference"});
    Ok(record)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_compact_outputs_complete_turn_non_regression() -> Result<()> {
    require_network!();
    let record = one_turn(false, None).await?;
    assert_eq!(record["model_requests"], 11);
    assert_eq!(record["recovery_calls"], 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "opt-in online candidate-on/off full-turn token benchmark"]
async fn candidates_complete_turn_non_regression() -> Result<()> {
    require_network!();
    live::assert_usage_parser_contract();
    live::assert_transport_bridge().await?;
    let baseline = one_turn(false, None).await?;
    let candidate = one_turn(true, None).await?;
    assert!(candidate["input_tokens"].as_u64() < baseline["input_tokens"].as_u64());
    assert!(candidate["total_tokens"].as_u64() < baseline["total_tokens"].as_u64());
    assert!(
        candidate["command_output_tokens"].as_u64() < baseline["command_output_tokens"].as_u64()
    );
    assert_eq!(candidate["model_requests"], baseline["model_requests"]);
    assert_eq!(candidate["recovery_calls"], baseline["recovery_calls"]);
    if let Some(path) = std::env::var_os("KD4_TOKEN_CACHE_E2E_OUTPUT") {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        for record in [baseline, candidate] {
            writeln!(file, "{record}")?;
        }
    }
    Ok(())
}
