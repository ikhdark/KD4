//! Test-only policy comparisons: findings 1, 2, 3, 9, 12, 15, 21, 23.
//! Timings include candidate transformations, but never imply live-model savings.
use super::*;
use crate::tool_history::ModelGenerationId;
use crate::tool_history::ToolHistoryCandidate;
use crate::tool_history::ToolHistoryState;
use crate::tools::command_output_artifact::*;
use crate::tools::context::ExecCommandToolOutput;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_tools::CanonicalToolResult;
use codex_tools::ToolOutput;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::time::Instant;

const SAMPLES: usize = 9;
const THREAD: &str = "output-recovery-benchmark";

fn tokens(text: &str) -> usize {
    codex_utils_output_truncation::model_token_count(text)
}

fn timing(mut samples: Vec<u64>) -> Value {
    let raw = samples.clone();
    samples.sort_unstable();
    json!({"samples_ns":raw,"median_ns":samples[samples.len()/2],
        "p95_ns":samples[(samples.len()*95/100).min(samples.len()-1)]})
}

async fn compare<F, Fut>(mut operation: F) -> Value
where
    F: FnMut(bool) -> Fut,
    Fut: Future<Output = Value>,
{
    // Alternate order to avoid always giving one policy warmer filesystem caches.
    let mut samples = [Vec::new(), Vec::new()];
    let mut outcomes = [Value::Null, Value::Null];
    for iteration in 0..=SAMPLES {
        for candidate in if iteration % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let start = Instant::now();
            let outcome = operation(candidate).await;
            let elapsed = u64::try_from(start.elapsed().as_nanos()).unwrap();
            let index = usize::from(candidate);
            if iteration > 0 {
                samples[index].push(elapsed);
            }
            if !outcomes[index].is_null() {
                assert_eq!(
                    outcomes[index], outcome,
                    "workload observations must be deterministic"
                );
            }
            outcomes[index] = outcome;
        }
    }
    json!({"baseline":{"timing":timing(samples[0].clone()),"observed":outcomes[0]},
        "candidate":{"timing":timing(samples[1].clone()),"observed":outcomes[1]}})
}

#[allow(clippy::print_stdout)]
fn emit(finding: Value, case: &str, comparison: Value, limitation: &str) {
    let record = json!({"finding":finding,"case":case,"comparison":comparison,
        "scope":"test-only policy comparison; production helpers; no model inference",
        "limitation":limitation});
    if let Some(path) = std::env::var_os("OUTPUT_RECOVERY_BENCH_OUTPUT") {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
    }
    println!("OUTPUT_RECOVERY_BENCH {record}");
}

fn source(rows: usize) -> String {
    (0..rows)
        .map(|i| {
            format!("ROW_{i:04}: exact source evidence for operation {i}; preserve this result.\n")
        })
        .collect()
}

fn outer(text: String, budget: Option<usize>) -> (String, bool) {
    let (items, truncated) = truncate_code_mode_result(
        vec![FunctionCallOutputContentItem::InputText { text }],
        budget,
        OutputOutcome::Success,
        10_000,
        None,
    );
    (code_mode_text_content(&items), truncated)
}

fn command(
    source: &str,
    artifact: RawOutputArtifact,
    limit: Option<usize>,
) -> ExecCommandToolOutput {
    ExecCommandToolOutput {
        validation: None,
        event_call_id: "bench-command".into(),
        chunk_id: "bench-chunk".into(),
        wall_time: Duration::ZERO,
        raw_output: source.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: limit,
        process_id: Some(123),
        session_capabilities: None,
        exit_code: None,
        process_exited: false,
        search_no_match: false,
        original_token_count: None,
        hook_command: None,
        raw_output_artifact: Some(artifact),
        raw_output_reduction_notice: None,
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    }
}

async fn nested_budgets(home: &Path) {
    let source = source(450);
    let artifact = create_raw_output_artifact(home, THREAD, source.as_bytes()).await;
    let payload = ToolPayload::Function {
        arguments: "{}".into(),
    };
    for (finding, count) in [(1, 1), (2, 1), (2, 2), (2, 4)] {
        let comparison = compare(|candidate| {
            let source = &source;
            let artifact = &artifact;
            let payload = &payload;
            async move {
                let inner_limit = (candidate && finding == 2).then_some(5_000 / count);
                let value = command(source, artifact.clone(), inner_limit).code_mode_result(payload);
                assert_eq!(value["session_id"], 123);
                assert_eq!(value["output_complete"], false);
                assert_eq!(value["raw_output_artifact_id"], json!(artifact.model_projection().0.map(|id| id.to_string())));
                let wire = vec![value.clone(); count].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
                let budget = if finding == 1 && !candidate { None } else { Some(10_000) };
                let (visible, truncated) = outer(wire, budget);
                if candidate { assert!(!truncated, "candidate packet should fit"); }
                let inline_rows = (0..450).filter(|i| visible.contains(&format!("ROW_{i:04}"))).count();
                json!({"outer_truncations":usize::from(truncated),"nested_reductions":usize::from(value["output_reduced"] == true)*count,
                    "visible_tokens":tokens(&visible),"unique_rows_inline":inline_rows,"required_middle_inline":visible.contains("ROW_0225"),
                    "nested_results":count,"canonical_rows_per_result":450})
            }
        }).await;
        emit(
            json!(finding),
            &format!("nested_results_{count}"),
            comparison,
            "#1 raises only the outer default to its existing hard cap. #2 allocates a fixed shared inner allowance; fewer outer cuts may also mean less inline evidence. No negotiation is installed.",
        );
    }
    let snapshot = load_tool_output_snapshot(
        home,
        THREAD,
        &artifact.model_projection().0.unwrap().to_string(),
    )
    .await
    .unwrap();
    let recovered = snapshot
        .select(
            vec![ToolOutputSelector::Lines {
                start: 226,
                end: 226,
            }],
            2_000,
        )
        .await
        .unwrap();
    assert_eq!(
        recovered.results[0].text.as_deref(),
        source
            .lines()
            .nth(225)
            .map(|line| format!("{line}\n"))
            .as_deref()
    );
}

async fn bounded_page(
    snapshot: &Arc<ToolOutputSnapshot>,
    end: u64,
    budget: usize,
) -> ReadToolOutputResult {
    // A deliberately simple prototype; selection cost and every probe are timed.
    let mut end = end;
    loop {
        let result = snapshot
            .select(vec![ToolOutputSelector::Bytes { start: 0, end }], budget)
            .await
            .unwrap();
        if result.complete && tokens(&serde_json::to_string(&result).unwrap()) <= budget {
            return result;
        }
        assert!(end > 1);
        end /= 2;
    }
}

async fn recovery_budget(home: &Path) {
    let text = source(240);
    let canonical = CanonicalToolResult::text(text.clone());
    let artifact = create_canonical_output_artifact(home, THREAD, &canonical).await;
    let id = artifact.artifact_id().unwrap();
    for budget in [2_000, 4_000, 10_000] {
        let comparison = compare(|candidate| {
            let text = &text;
            let id = &id;
            async move {
                let snapshot = load_tool_output_snapshot(home, THREAD, id).await.unwrap();
                let result = if candidate {
                    bounded_page(&snapshot, text.len() as u64, budget - 256).await
                } else {
                    snapshot.select(vec![ToolOutputSelector::Bytes { start: 0, end: text.len() as u64 }], 9_000).await.unwrap()
                };
                assert!(result.complete);
                let delivered = result.results[0].text.as_ref().unwrap();
                assert!(text.starts_with(delivered));
                let delivered_bytes = delivered.len();
                let mut packet = serde_json::to_value(&result).unwrap();
                if delivered_bytes < text.len() {
                    packet["continuation"] = json!({"kind":"bytes","start":delivered_bytes,"end":text.len()});
                }
                let (visible, truncated) = outer(packet.to_string(), Some(budget));
                if candidate {
                    assert!(!truncated);
                    assert!(serde_json::from_str::<Value>(&visible).is_ok());
                }
                json!({"outer_truncations":usize::from(truncated),"artifact_loads":1,
                    "selected_bytes":delivered_bytes,"remaining_bytes":text.len()-delivered_bytes,
                    "visible_tokens":tokens(&visible),"full_source_delivered":delivered_bytes==text.len() && !truncated})
            }
        }).await;
        emit(
            json!(3),
            &format!("outer_budget_{budget}"),
            comparison,
            "Candidate is a bounded prefix with an explicit remaining range, not complete recovery. One artifact load per policy; page-size probing is included.",
        );
    }
    assert!(
        load_tool_output_snapshot(home, "wrong-thread", &id)
            .await
            .is_err()
    );
}

async fn in_memory_selection(home: &Path) {
    for rows in [500, 4_000] {
        let text = source(rows);
        let canonical = CanonicalToolResult::text(text);
        let artifact = create_canonical_output_artifact(home, THREAD, &canonical).await;
        let id = artifact.artifact_id().unwrap();
        let comparison = compare(|candidate| {
            let canonical = &canonical;
            let id = &id;
            async move {
                let selectors = vec![ToolOutputSelector::Lines { start: 40, end: 50 }];
                let result = if candidate {
                    select_file_snapshot(canonical, Some(selectors)).unwrap().0
                } else {
                    read_tool_output_selectors_with_ceiling_and_reuse(home, THREAD, id, selectors, 10_000).await.unwrap().0
                };
                assert!(result.complete);
                let expected = String::from_utf8(canonical.bytes.clone()).unwrap().lines().skip(39).take(11).map(|line| format!("{line}\n")).collect::<String>();
                assert_eq!(result.results[0].text.as_deref(), Some(expected.as_str()));
                json!({"artifact_loads":usize::from(!candidate),"source_bytes":canonical.exact_bytes,"selected_bytes":expected.len(),
                    "evidence_sha256":crate::tool_history::sha256(expected.as_bytes())})
            }
        }).await;
        emit(
            json!(9),
            &format!("source_rows_{rows}"),
            comparison,
            "Artifact creation is common setup. Candidate includes rebuilding producer metadata through select_file_snapshot; disk baseline uses a warm OS cache.",
        );
    }
}

fn search(query: &str) -> ToolOutputSelector {
    ToolOutputSelector::Search {
        query: query.into(),
        start_byte: 0,
        max_results: 100,
        context_lines: 0,
    }
}

async fn search_budget(home: &Path) {
    let text = ["ALPHA", "BETA", "GAMMA"]
        .iter()
        .flat_map(|query| {
            (0..36)
                .map(move |i| format!("{query} {i:03} {}\n", "exact matching context ".repeat(5)))
        })
        .collect::<String>();
    let canonical = CanonicalToolResult::text(text.clone());
    let artifact = create_canonical_output_artifact(home, THREAD, &canonical).await;
    let id = artifact.artifact_id().unwrap();
    let snapshot = load_tool_output_snapshot(home, THREAD, &id).await.unwrap();
    let comparison = compare(|candidate| {
        let snapshot = &snapshot;
        let text = &text;
        async move {
            let result = if candidate {
                let mut response = snapshot.select(vec![search("ALPHA")], 1_000).await.unwrap();
                response.results.clear();
                for (index, query) in ["ALPHA", "BETA", "GAMMA"].iter().enumerate() {
                    let occupied = codex_utils_string::approx_token_count(&serde_json::to_string(&response).unwrap());
                    let remaining = 4_000_usize.saturating_sub(occupied + 200);
                    let page = snapshot.select(vec![search(query)], remaining / (3-index)).await.unwrap();
                    response.results.extend(page.results);
                }
                response.complete = response.results.iter().all(|r| r.complete);
                response
            } else {
                snapshot.select(vec![search("ALPHA"), search("BETA"), search("GAMMA")], 4_000).await.unwrap()
            };
            let counts = result.results.iter().map(|r| r.value.as_ref().and_then(|v| v["matches_returned"].as_u64()).unwrap_or(0)).collect::<Vec<_>>();
            for selection in &result.results {
                if let Some(ranges) = selection.value.as_ref().and_then(|v| v["hydrated_ranges"].as_array()) {
                    for range in ranges {
                        let start = range["canonical_range"]["start"].as_u64().unwrap() as usize;
                        let end = range["canonical_range"]["end"].as_u64().unwrap() as usize;
                        assert_eq!(range["text"].as_str().unwrap(), &text[start..end]);
                    }
                }
            }
            if candidate { assert!(counts.iter().all(|count| *count > 0)); }
            let wire = serde_json::to_string(&result).unwrap();
            let (_, truncated) = outer(wire.clone(), Some(4_000));
            assert!(!truncated);
            json!({"matches_per_query":counts,"queries_with_evidence":counts.iter().filter(|n| **n > 0).count(),
                "aggregate_omitted":result.results.iter().filter(|r| r.status==ToolOutputSelectorStatus::AggregateOmitted).count(),
                "wire_tokens":tokens(&wire),"all_search_pages_complete":result.complete})
        }
    }).await;
    emit(
        json!(12),
        "three_searches_shared_4000_budget",
        comparison,
        "Allocation is test-only and includes extra selector invocations. Counts measure delivered matches, not completed searches; continuation remains necessary.",
    );
}

fn compact_recovery(result: &ReadToolOutputResult) -> Value {
    let mut value = serde_json::to_value(result).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("delivered_selection_complete"); // alias of complete
    object.remove("retained_artifact_complete"); // derivable from sizes/unavailable ranges
    for selection in value["results"].as_array_mut().unwrap() {
        let object = selection.as_object_mut().unwrap();
        if object.get("complete") == Some(&Value::Bool(true)) {
            object.remove("child_selectors");
            object.remove("subdivision_plan");
            object.remove("message");
        }
        if let Some(text) = object.get("text").and_then(Value::as_str) {
            assert_eq!(
                object.get("exact_bytes").and_then(Value::as_u64),
                Some(text.len() as u64)
            );
            object.remove("exact_bytes");
        }
        if object["selector"]["kind"] == "bytes" {
            object.remove("canonical_range");
        }
    }
    value
}

async fn metadata_projection(home: &Path) {
    let canonical = CanonicalToolResult::text(source(200));
    let id = create_canonical_output_artifact(home, THREAD, &canonical)
        .await
        .artifact_id()
        .unwrap();
    for count in [1, 32] {
        let selectors = (0..count)
            .map(|i| ToolOutputSelector::Lines {
                start: i * 3 + 1,
                end: i * 3 + 1,
            })
            .collect();
        let result =
            read_tool_output_selectors_with_ceiling_and_reuse(home, THREAD, &id, selectors, 10_000)
                .await
                .unwrap()
                .0;
        assert!(result.complete);
        let comparison = compare(|candidate| {
            let result = &result;
            let id = &id;
            async move {
                let output = if candidate { compact_recovery(result) } else { serde_json::to_value(result).unwrap() };
                assert_eq!(output["artifact_id"].as_str(), Some(id.as_str()));
                assert_eq!(output["canonical_sha256"], result.canonical_sha256);
                assert_eq!(output["complete"], result.complete);
                for (original, projected) in result.results.iter().zip(output["results"].as_array().unwrap()) {
                    assert_eq!(projected["text"].as_str(), original.text.as_deref());
                    assert_eq!(projected["selector"], serde_json::to_value(&original.selector).unwrap());
                    assert_eq!(projected.get("continuation"), serde_json::to_value(original).unwrap().get("continuation"));
                }
                let wire = output.to_string();
                json!({"wire_tokens":tokens(&wire),"wire_bytes":wire.len(),"exact_selections":result.results.len()})
            }
        }).await;
        emit(
            json!(15),
            &format!("complete_selections_{count}"),
            comparison,
            "Model-view prototype only. Native tool schema and JS result are unchanged; token savings do not imply fewer calls unless a packet crosses its limit.",
        );
    }
    let result = read_tool_output_selectors_with_ceiling_and_reuse(
        home,
        THREAD,
        &id,
        vec![ToolOutputSelector::Lines { start: 0, end: 1 }],
        2_000,
    )
    .await
    .unwrap()
    .0;
    let projected = compact_recovery(&result);
    assert_eq!(projected["results"][0]["status"], "invalid");
    assert!(projected["results"][0]["message"].is_string());
}

async fn multi_artifact(home: &Path) {
    let mut ids = Vec::new();
    for index in 0..4 {
        let canonical = CanonicalToolResult::text(format!(
            "ARTIFACT_{index}: independently verified evidence\n"
        ));
        ids.push(
            create_canonical_output_artifact(home, THREAD, &canonical)
                .await
                .artifact_id()
                .unwrap(),
        );
    }
    let comparison = compare(|candidate| {
        let ids = &ids;
        async move {
            let mut outputs = Vec::new();
            for (index, id) in ids.iter().enumerate() {
                let result = read_tool_output_selectors_with_ceiling_and_reuse(home, THREAD, id, vec![ToolOutputSelector::Lines {start:1,end:1}], 2_000).await.unwrap().0;
                assert!(result.complete);
                assert!(result.results[0].text.as_ref().unwrap().contains(&format!("ARTIFACT_{index}")));
                outputs.push(serde_json::to_value(result).unwrap());
            }
            let wire = if candidate {json!({"artifacts":outputs}).to_string()} else {outputs.iter().map(Value::to_string).collect::<Vec<_>>().join("\n")};
            assert!(tokens(&wire) < 10_000);
            json!({"artifact_loads":4,"logical_api_requests":if candidate {1}else{4},"wire_tokens":tokens(&wire),"artifacts_verified":4})
        }
    }).await;
    emit(
        json!(21),
        "four_artifacts",
        comparison,
        "A test adapter groups four reads, not a registered multi-artifact API. It does NOT reduce disk loads or prove fewer model turns; baseline calls can already share one code-mode cell.",
    );
}

fn output_item(id: &str, text: String) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: id.into(),
        output: FunctionCallOutputPayload::from_text(text),
        internal_chat_message_metadata_passthrough: None,
    }
}

async fn history_policy(home: &Path) {
    let mut state = ToolHistoryState::default();
    let mut pairs = Vec::new();
    let mut ids = Vec::new();
    let mut texts = Vec::new();
    for index in 0..12 {
        let id = format!("history-{index}");
        let text = format!("DOCUMENT_{index}\n{}", source(45));
        let canonical = CanonicalToolResult::text(text.clone());
        let artifact_id = create_canonical_output_artifact(home, THREAD, &canonical)
            .await
            .artifact_id()
            .unwrap();
        state.register(ToolHistoryCandidate {
            call_id: id.clone(),
            tool_identity: "functions.exec".into(),
            semantic_class: "tool_output".into(),
            successful: index != 11,
            source_dependencies: BTreeSet::new(),
            source_dependencies_current: true,
            artifact_id: artifact_id.clone(),
            artifact_bytes: canonical.exact_bytes,
            artifact_sha256: canonical.sha256,
            original_output_sha256: crate::tool_history::sha256(text.as_bytes()),
            original_tokens: tokens(&text) as u64,
            preserved_non_text_tokens: Some(0),
            bounded_model_output: text.clone(),
            complete: true,
            projection_eligible: true,
            proof_identity: None,
            supersession_identity: None,
            consumed_by_generation: Some(ModelGenerationId {
                turn_id: "history-bench".into(),
                ordinal: index,
            }),
            derived: Default::default(),
        });
        state.register_non_workspace_code_mode_call(id.clone());
        pairs.push(vec![
            ResponseItem::FunctionCall {
                id: None,
                name: "functions.exec".into(),
                namespace: None,
                arguments: "{}".into(),
                call_id: id.clone(),
                internal_chat_message_metadata_passthrough: None,
            },
            output_item(&id, text.clone()),
        ]);
        ids.push(artifact_id);
        texts.push(text);
    }
    for budget in [4_000, 8_000, 16_000] {
        state.set_model_visible_tool_result_token_budget(Some(budget));
        let comparison = compare(|candidate| {
        let state = &state;
        let pairs = &pairs;
        let ids = &ids;
        let texts = &texts;
        async move {
            // Prototype admission ranking only: preserve the same evidence budget.
            // Reordering is not installed in production or presented as a safe history rewrite.
            let order = if candidate {(2..12).chain(0..2).collect::<Vec<_>>()} else {(0..12).collect()};
            let items = order.iter().flat_map(|i| pairs[*i].clone()).collect::<Vec<_>>();
            let projection = state.clone().project(items.into());
            assert!(projection.items.contains(&output_item("history-11",texts[11].clone())),"active failure must stay visible");
            let mut recovery_calls = 0;
            for index in 0..2 {
                if !projection.items.contains(&output_item(&format!("history-{index}"),texts[index].clone())) {
                    let result = read_tool_output_selectors_with_ceiling_and_reuse(home, THREAD, &ids[index],vec![ToolOutputSelector::Lines {start:1,end:1}],2_000).await.unwrap().0;
                    assert_eq!(result.results[0].text.as_deref(),Some(format!("DOCUMENT_{index}\n").as_str()));
                    recovery_calls += 1;
                }
            }
            let raw_documents = (0..12).filter(|i| projection.items.contains(&output_item(&format!("history-{i}"), texts[*i].clone()))).count();
            let cold_evidence_missing = [9,10].iter().filter(|i| !projection.items.contains(&output_item(&format!("history-{i}"), texts[**i].clone()))).count();
            json!({"hot_evidence_recovery_calls":recovery_calls,"artifact_loads":recovery_calls,"task_success":true,"evidence_budget":budget,
                "raw_documents_retained":raw_documents,"cold_evidence_missing":cold_evidence_missing})
        }
    }).await;
        emit(
            json!(23),
            &format!("revisit_two_older_documents_budget_{budget}"),
            comparison,
            "Uses the production compaction projector with an oracle hot-set admission ordering. Ordinary sampling is a different path; real access tracking and prediction quality are not implemented.",
        );
    }
}

async fn pipeline(home: &Path) {
    let text = source(450);
    let canonical = CanonicalToolResult::text(text.clone());
    let comparison = compare(|candidate| {
        let text = &text;
        let canonical = &canonical;
        async move {
            // Real filesystem persistence -> command projection -> outer packet -> exact recovery.
            let artifact = create_raw_output_artifact(home, THREAD, text.as_bytes()).await;
            let id = artifact.model_projection().0.unwrap().to_string();
            let payload = ToolPayload::Function {arguments:"{}".into()};
            let value = command(text,artifact,None).code_mode_result(&payload);
            let (visible,truncated) = outer(value.to_string(),candidate.then_some(10_000));
            let mut loads = 0;
            let mut recovery_calls = 0;
            let evidence = if visible.contains("ROW_0225") {visible} else {
                loads += 1;
                recovery_calls += 1;
                let recovered = read_tool_output_selectors_with_ceiling_and_reuse(home,THREAD,&id,vec![ToolOutputSelector::Lines {start:226,end:226}],2_000).await.unwrap().0;
                assert!(recovered.complete);
                recovered.results[0].text.clone().unwrap()
            };
            assert!(evidence.contains("ROW_0225"));
            // An automatic excerpt uses the same producer bytes in the candidate.
            let selectors = vec![ToolOutputSelector::Lines {start:1,end:1}];
            let preset = if candidate {select_file_snapshot(canonical,Some(selectors)).unwrap().0} else {
                loads += 1;
                read_tool_output_selectors_with_ceiling_and_reuse(home,THREAD,&id,selectors,10_000).await.unwrap().0
            };
            assert!(preset.complete);
            assert!(preset.results[0].text.as_ref().unwrap().contains("ROW_0000"));
            let final_packet = if candidate {compact_recovery(&preset)} else {serde_json::to_value(&preset).unwrap()};
            let (_,retruncated) = outer(final_packet.to_string(),Some(4_000));
            assert!(!retruncated);
            json!({"outer_truncations":usize::from(truncated),"artifact_loads":loads,"recovery_calls":recovery_calls,
                "task_success":true,"producer_executions":1,"required_markers":2})
        }
    }).await;
    emit(
        json!("pipeline"),
        "filesystem_to_projection_to_verified_evidence",
        comparison,
        "Combined deterministic evidence pipeline exercises #1/#3/#9/#15. Other findings have separate workloads; this is not an eight-change full-stack A/B or a live-model turn.",
    );
}

#[tokio::test]
#[ignore = "opt-in ordered output/recovery benchmark"]
async fn ordered_output_recovery_benchmark() {
    let home = tempfile::tempdir().unwrap();
    nested_budgets(home.path()).await;
    recovery_budget(home.path()).await;
    in_memory_selection(home.path()).await;
    search_budget(home.path()).await;
    metadata_projection(home.path()).await;
    multi_artifact(home.path()).await;
    history_policy(home.path()).await;
    pipeline(home.path()).await;
}
