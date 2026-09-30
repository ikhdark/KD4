//! Opt-in, ordered measurements of review findings 15, 17, 5, 2, 10, 18.
//! Candidates are deliberately test-only. No provider cache hits or model behavior
//! are inferred from payload size, tokenizer counts, or local timings.
use super::*;
use crate::git_workspace::GitWorkspaceCache;
use crate::git_workspace::WorkspaceEvidenceIdentity;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::tool_history::ToolHistoryState;
use crate::tool_history::WorkspaceEvidenceObservation;
use crate::tools::command_output_artifact::ToolOutputSelector;
use crate::tools::command_output_artifact::create_raw_output_artifact;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::handlers::ReadFileHandler;
use crate::tools::handlers::ToolSearchHandlerCache;
use crate::tools::handlers::execute_recovery_transaction;
use crate::tools::router::ToolRouter;
use crate::tools::router::ToolRouterParams;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_features::Feature;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolExecutor;
use codex_tools::ToolOutput;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::hint::black_box;
use std::time::Instant;
use tokio_util::sync::CancellationToken;

fn tokens(text: &str) -> usize {
    codex_utils_output_truncation::model_token_count(text)
}

fn measure(mut operation: impl FnMut() -> String) -> Value {
    for _ in 0..3 {
        black_box(operation());
    }
    let mut samples = Vec::new();
    for _ in 0..21 {
        let start = Instant::now();
        for _ in 0..8 {
            black_box(operation());
        }
        samples.push(u64::try_from(start.elapsed().as_nanos() / 8).unwrap());
    }
    let raw = samples.clone();
    samples.sort_unstable();
    json!({"samples_ns":raw,"median_ns":samples[10],"p95_ns":samples[19]})
}

#[allow(clippy::print_stdout)]
fn emit(finding: u32, case: &str, baseline: &str, candidate: &str, details: Value) {
    let record = json!({"finding":finding,"case":case,
            "baseline_tokens":tokens(baseline),"candidate_tokens":tokens(candidate),
            "baseline_bytes":baseline.len(),"candidate_bytes":candidate.len(),
            "details":details,"scope":"test-only projection; no live-model or provider-cache measurement"});
    println!("TOKEN_CACHE_BENCH {record}");
    if let Some(path) =
        std::env::var_os("KD4_TOKEN_CACHE_BENCH_OUTPUT").filter(|path| !path.is_empty())
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
    }
}

async fn invocation(tool_name: &str, arguments: Value) -> ToolInvocation {
    let (session, mut turn) = make_session_and_context().await;
    turn.permission_profile = PermissionProfile::Disabled;
    ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::new(turn)),
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new())),
        call_id: format!("bench-{tool_name}"),
        tool_name: ToolName::plain(tool_name),
        source: ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: arguments.to_string(),
        },
    }
}

fn outer(text: String) -> String {
    let result = format_runtime_response(
        RuntimeResponse::Result {
            cell_id: CellId::new("token-cache-bench".into()),
            content_items: vec![codex_code_mode::FunctionCallOutputContentItem::InputText { text }],
            error_text: None,
            output_loss: None,
        },
        Some(4_000),
        10_000,
        false,
        Instant::now(),
        Vec::new(),
        Vec::new(),
        None,
    );
    code_mode_text_content(&result.body)
}

async fn nested_budget() {
    let home = tempfile::tempdir().unwrap();
    let source = (0..800)
        .map(|i| format!("ROW_{i:04}: exact evidence and diagnostics for source {i}\n"))
        .collect::<String>();
    let artifact = create_raw_output_artifact(home.path(), "nested", source.as_bytes()).await;
    let base = ExecCommandToolOutput {
        validation: None,
        event_call_id: "command".into(),
        chunk_id: "chunk".into(),
        wall_time: Duration::ZERO,
        raw_output: source.as_bytes().to_vec(),
        truncation_policy: TruncationPolicy::Tokens(10_000),
        max_output_tokens: None,
        process_id: Some(123),
        session_capabilities: None,
        exit_code: None,
        process_exited: false,
        search_no_match: false,
        original_token_count: None,
        hook_command: None,
        raw_output_artifact: Some(artifact),
        repair_notice: None,
        pending_deferred_completions: Vec::new(),
    };
    let payload = ToolPayload::Function {
        arguments: "{}".into(),
    };
    for count in [1, 2] {
        let candidate = ExecCommandToolOutput {
            max_output_tokens: Some(2_500 / count),
            ..base.clone()
        };
        let before = base.code_mode_result(&payload);
        let after = candidate.code_mode_result(&payload);
        for value in [&before, &after] {
            assert_eq!(value["session_id"], 123);
            assert_eq!(value["execution_state"], "running");
            assert_eq!(value["output_complete"], false);
            let id = value["raw_output_artifact_id"].as_str().unwrap();
            let (recovered, _) = execute_recovery_transaction(
                home.path(),
                "nested",
                id,
                vec![ToolOutputSelector::Lines {
                    start: 395,
                    end: 405,
                }],
                true,
            )
            .await
            .unwrap();
            assert!(recovered.complete);
            assert!(
                recovered.results[0]
                    .text
                    .as_ref()
                    .unwrap()
                    .contains("ROW_0400")
            );
        }
        let baseline_wire = vec![before; count]
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let candidate_wire = vec![after; count]
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let baseline_outer = outer(baseline_wire.clone());
        let candidate_outer = outer(candidate_wire.clone());
        assert!(tokens(&candidate_wire) < tokens(&baseline_wire));
        assert!(candidate_outer.contains("ROW_0000"));
        emit(
            15,
            &format!("{count}_running_commands"),
            &baseline_wire,
            &candidate_wire,
            json!({"baseline_outer_tokens":tokens(&baseline_outer),"candidate_outer_tokens":tokens(&candidate_outer),
                "baseline_outer":measure(||outer(baseline_wire.clone())),
                "candidate_outer":measure(||outer(candidate_wire.clone())),
                "recovery_verified":true,"limitation":"Smaller inline evidence may require more recovery; full-turn savings not established."}),
        );
    }
}

fn compact_file_envelope(value: &Value) -> Value {
    let mut result = value.clone();
    let fields = result.as_object_mut().unwrap();
    assert_eq!(fields["source_sha256"], fields["canonical_sha256"]);
    assert_eq!(fields["complete"], fields["delivered_selection_complete"]);
    fields.remove("canonical_sha256");
    fields.remove("delivered_selection_complete");
    if fields.get("artifact_id").is_some_and(Value::is_null) {
        fields.remove("artifact_id");
    }
    result
}

async fn file_envelope() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("source.txt");
    std::fs::write(&path, "first line\nUnicode: λ 日本語\nlast line\n").unwrap();
    for (case, selectors) in [
        ("whole", None),
        (
            "selected",
            Some(json!([{"kind":"lines","start":2,"end":2}])),
        ),
    ] {
        let mut args = json!({"path":path});
        if let Some(selectors) = selectors {
            args["selectors"] = selectors;
        }
        let call = invocation("read_file", args).await;
        let payload = call.payload.clone();
        let baseline = ReadFileHandler
            .handle(call)
            .await
            .unwrap()
            .code_mode_result(&payload);
        let candidate = compact_file_envelope(&baseline);
        let mut restored = candidate.clone();
        restored["canonical_sha256"] = restored["source_sha256"].clone();
        restored["delivered_selection_complete"] = restored["complete"].clone();
        if restored.get("artifact_id").is_none() {
            restored["artifact_id"] = Value::Null;
        }
        assert_eq!(restored, baseline);
        assert_eq!(candidate["file_complete"], case == "whole");
        assert!(
            candidate["results"][0]["text"]
                .as_str()
                .unwrap()
                .contains("日本語")
        );
        emit(
            17,
            case,
            &baseline.to_string(),
            &candidate.to_string(),
            json!({
            "roundtrip_exact":true,"baseline_render":measure(||baseline.to_string()),
            "candidate_transform_and_render":measure(||compact_file_envelope(&baseline).to_string())}),
        );
    }
}

fn search_receipt(current: &Value, previous: Option<&Value>) -> Value {
    if previous != Some(current) {
        return current.clone();
    }
    json!({"already_available":true,"schema_sha256":format!("{:x}",Sha256::digest(current.to_string().as_bytes()))})
}

async fn repeated_search() {
    let cache = ToolSearchHandlerCache::default();
    let info = ToolSearchInfo::from_tool_spec(&ReadFileHandler.spec(), None).unwrap();
    let handler = cache.get_or_build(vec![info]);
    let mut call = invocation("tool_search", json!({"query":"read_file","limit":1})).await;
    call.payload = ToolPayload::ToolSearch {
        arguments: serde_json::from_value(json!({"query":"read_file","limit":1})).unwrap(),
    };
    let payload = call.payload.clone();
    // Both requests use one real turn's activation state and one search cache.
    let first = handler
        .handle(call.clone())
        .await
        .unwrap()
        .code_mode_result(&payload);
    let second = handler
        .handle(call)
        .await
        .unwrap()
        .code_mode_result(&payload);
    assert_eq!(first, second);
    assert!(!first["tools"].as_array().unwrap().is_empty());
    let candidate = search_receipt(&second, Some(&first));
    assert_eq!(
        search_receipt(&second, None),
        second,
        "lost history must restore the schema"
    );
    let changed = json!({"tools":[{"name":"read_file","revision":"new"}]});
    assert_eq!(search_receipt(&changed, Some(&first)), changed);
    emit(
        5,
        "same_query_same_turn",
        &second.to_string(),
        &candidate.to_string(),
        json!({
        "first_response_tokens":tokens(&first.to_string()),"lost_history_and_revision_guards":true,
        "baseline_render":measure(||second.to_string()),
        "candidate_transform_and_render":measure(||search_receipt(&second,Some(&first)).to_string()),
        "limitation":"No production visibility ledger or receipt resolver is installed."}),
    );
}

async fn mixed_schemas() {
    let (_session, mut turn) = make_session_and_context().await;
    let mut config = (*turn.config).clone();
    for feature in [Feature::CodeMode, Feature::ShellTool, Feature::UnifiedExec] {
        config.features.enable(feature).unwrap();
    }
    config.features.disable(Feature::CodeModeOnly).unwrap();
    turn.config = Arc::new(config);
    turn.permission_profile = PermissionProfile::Disabled;
    turn.model_info.supports_search_tool = false;
    let step = StepContext::for_test(Arc::new(turn));
    let router = ToolRouter::from_context(
        &step,
        ToolRouterParams {
            mcp_tools: None,
            deferred_mcp_tools: None,
            tool_suggest_candidates: None,
            extension_tool_executors: Vec::new(),
            dynamic_tools: &[],
            exposure_identity: Default::default(),
        },
        &ToolSearchHandlerCache::default(),
    );
    let baseline = router.model_visible_specs();
    assert!(baseline.iter().any(|spec| spec.name() == "read_file"));
    let mut candidate = baseline.clone();
    let mut removed = false;
    for (original, projected) in baseline.iter().zip(&mut candidate) {
        if let ToolSpec::Freeform(tool) = projected
            && tool.name == PUBLIC_TOOL_NAME
        {
            let (prefix, declarations) = tool
                .description
                .split_once("\n\nEager nested tool contracts:")
                .unwrap();
            assert!(declarations.contains("read_file(args:"));
            tool.description = format!(
                "{prefix}\n\nDirect tool contracts are advertised separately. Resolve missing nested declarations with resolve_tool(name)."
            );
            removed = true;
        } else {
            assert_eq!(
                serde_json::to_value(original).unwrap(),
                serde_json::to_value(projected).unwrap()
            );
        }
    }
    assert!(removed);
    let serialize = |specs: &Vec<ToolSpec>| serde_json::to_string(specs).unwrap();
    emit(
        2,
        "mixed_builtin_router",
        &serialize(&baseline),
        &serialize(&candidate),
        json!({
        "tool_count_unchanged":baseline.len(),"baseline_render":measure(||serialize(&baseline)),
        "candidate_render":measure(||serialize(&candidate)),
        "limitation":"Potential resolve_tool/model handoffs are not measured by schema serialization."}),
    );
}

async fn invalidation_notices() {
    for count in [1, 8, 32] {
        let mut state = ToolHistoryState::default();
        let mut history = Vec::new();
        for index in 0..count {
            let call_id = format!("read-{index}");
            let output = ResponseItem::FunctionCallOutput {
                id: None,
                call_id: call_id.clone(),
                output: FunctionCallOutputPayload::from_text(format!("historical source {index}")),
                internal_chat_message_metadata_passthrough: None,
            };
            state.register_workspace_evidence(
                WorkspaceEvidenceObservation::from_response_item(
                    Some(WorkspaceEvidenceIdentity {
                        unavailable: false,
                        repository_root: None,
                        head_identity: Some("before".into()),
                        index_identity: None,
                        worktree_identity: None,
                    }),
                    &output,
                    BTreeSet::new(),
                )
                .unwrap(),
            );
            history.push(ResponseItem::FunctionCall {
                id: None,
                name: "read_file".into(),
                namespace: None,
                arguments: json!({"path":format!("file-{index}.rs")}).to_string(),
                call_id,
                internal_chat_message_metadata_passthrough: None,
            });
            history.push(output);
        }
        let original: Arc<[ResponseItem]> = history.into();
        let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
        let projected =
            state.project_sampling_with_workspace_cache(Arc::clone(&original), None, &cache);
        assert_eq!(&projected.items[..original.len()], original.as_ref());
        let notices = projected.items[original.len()..]
            .iter()
            .map(|item| {
                let ResponseItem::Message { content, .. } = item else {
                    panic!("notice message")
                };
                let ContentItem::InputText { text } = &content[0] else {
                    panic!("notice text")
                };
                text.clone()
            })
            .collect::<Vec<_>>();
        assert_eq!(notices.len(), 1);
        let (prefix, _) = notices[0].split_once("\n{").unwrap();
        let suffix = "\n</workspace_evidence_invalidation>";
        let batch = serde_json::from_str::<Value>(
            notices[0]
                .strip_prefix(&format!("{prefix}\n"))
                .unwrap()
                .strip_suffix(suffix)
                .unwrap(),
        )
        .unwrap();
        let records = batch["notices"].as_array().unwrap();
        assert_eq!(records.len(), count);
        assert_eq!(notices[0], format!("{prefix}\n{batch}{suffix}"));
        for record in records {
            assert_eq!(record["valid_for_current_workspace"], false);
        }
        // Production already batches notices; reconstruct the unbatched control.
        let render_baseline = || {
            records
                .iter()
                .map(|record| format!("{prefix}\n{record}{suffix}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let baseline = render_baseline();
        let candidate = notices[0].clone();
        if count > 1 {
            assert!(tokens(&candidate) < tokens(&baseline));
        }
        emit(
            10,
            &format!("{count}_invalidations"),
            &baseline,
            &candidate,
            json!({
            "all_records_roundtrip_exact":true,"historical_prefix_unchanged":true,
            "baseline_is_synthetic_unbatched":true,
            "baseline_render":measure(render_baseline),
            "candidate_render":measure(||format!("{prefix}\n{batch}{suffix}")),
            "limitation":"Serialization comparison only; model behavior and whole-turn performance are not measured."}),
        );
    }
}

fn mcp_projection(value: &Value) -> Value {
    let mut result = value.clone();
    let Some(structured) = result
        .get("structuredContent")
        .filter(|value| !value.is_null())
        .cloned()
    else {
        return result;
    };
    result["content"].as_array_mut().unwrap().retain(|item| {
        item["type"] != "text"
            || item["text"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .as_ref()
                != Some(&structured)
    });
    result
}

fn mcp_mirrors() {
    for rows in [0, 10, 100] {
        let data = json!({"rows":(0..rows).map(|i|json!({"id":i,"name":format!("record-{i}"),"status":"verified"})).collect::<Vec<_>>(),"complete":true});
        let result = codex_protocol::mcp::CallToolResult {
            content: vec![
                json!({"type":"text","text":"Caption must survive"}),
                json!({"type":"text","text":data.to_string()}),
            ],
            structured_content: Some(data),
            is_error: Some(rows == 0),
            meta: None,
        };
        let payload = ToolPayload::Function {
            arguments: "{}".into(),
        };
        let baseline = result.code_mode_result(&payload);
        let candidate = mcp_projection(&baseline);
        let decoded: codex_protocol::mcp::CallToolResult =
            serde_json::from_value(candidate.clone()).unwrap();
        assert_eq!(
            result.as_function_call_output_payload(),
            decoded.as_function_call_output_payload()
        );
        assert_eq!(candidate["content"].as_array().unwrap().len(), 1);
        assert_eq!(result.content.len(), 2, "JS result must not be mutated");
        let no_mirror =
            json!({"content":[{"type":"text","text":"caption"}],"structuredContent":null});
        assert_eq!(mcp_projection(&no_mirror), no_mirror);
        emit(
            18,
            &format!("{rows}_rows"),
            &baseline.to_string(),
            &candidate.to_string(),
            json!({
            "direct_payload_equivalent":true,"raw_js_result_unchanged":true,
            "baseline_render":measure(||baseline.to_string()),
            "candidate_transform_and_render":measure(||mcp_projection(&baseline).to_string())}),
        );
    }
}

#[tokio::test]
#[ignore = "opt-in ordered token/cache component benchmark"]
async fn ordered_token_cache_benchmark() {
    nested_budget().await;
    file_envelope().await;
    repeated_search().await;
    mixed_schemas().await;
    invalidation_notices().await;
    mcp_mirrors();
}
