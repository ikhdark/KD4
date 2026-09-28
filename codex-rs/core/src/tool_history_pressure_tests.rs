use super::*;
use pretty_assertions::assert_eq;

// Deterministic request/recovery replay, not a model-latency benchmark. Keep the
// production policy unchanged until a live-model workload confirms the tradeoff.
#[tokio::test]
async fn long_session_pressure_records_prompt_receipts_recovery_and_elapsed() {
    let home = tempfile::tempdir().unwrap();
    let thread = "pressure-replay";
    let mut state = ToolHistoryState::default();
    let mut history = Vec::new();
    let mut exact = BTreeMap::new();
    for i in 0..24 {
        let id = format!("completed-{i}");
        let text = format!("result {i}: source evidence\n").repeat(1600);
        let (mut entry, bytes) = stored_candidate(home.path(), thread, &id, text.clone()).await;
        entry.consumed_by_generation = Some(ModelGenerationId {
            turn_id: "observed".into(),
            ordinal: i,
        });
        exact.insert(id.clone(), (entry.artifact_id.clone(), bytes));
        state.register(entry);
        state.register_non_workspace_code_mode_call(id.clone());
        history.extend([function_call(&id), text_output(&id, text)]);
    }
    let active = "UNRESOLVED evidence required for the next edit\n".repeat(30);
    let (mut entry, _) = stored_candidate(home.path(), thread, "active", active.clone()).await;
    entry.successful = false;
    state.register(entry);
    state.register_non_workspace_code_mode_call("active".into());
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    for budget in [129_000, 100_000, 75_000, 8_000] {
        state.set_model_visible_tool_result_token_budget(Some(budget));
        let started = std::time::Instant::now();
        let mut input_tokens = 0;
        let mut receipt_tokens = 0;
        let mut recovery_calls = 0;
        for count in [8, 16, 24] {
            let mut request = history[..count * 2].to_vec();
            request.extend([
                function_call("active"),
                text_output("active", active.clone()),
            ]);
            request.push(ResponseItem::Message {
                id: None, role: "developer".into(), content: vec![codex_protocol::models::ContentItem::InputText {
                    text: format!("<completed_phase_checkpoint>\n{}\n</completed_phase_checkpoint>",
                        serde_json::json!({"receipts":state.phase_checkpoint_receipts(&["completed-0".into()]).unwrap(), "retained_evidence":["active"]})),
                }], phase: None, internal_chat_message_metadata_passthrough: None,
            });
            let sampling =
                state.project_sampling_with_workspace_cache(Arc::from(request), None, &cache);
            // Exercise the real aggregate-budget admission path on prepared history.
            let projected = state.project(Arc::from(sampling.items.to_vec()));
            let visible = projected
                .items
                .iter()
                .filter_map(canonical_textual_output_identity)
                .collect::<BTreeMap<_, _>>();
            assert_eq!(visible.get("active").unwrap().as_ref(), active);
            let mut request_tokens = 0;
            for (id, body) in &visible {
                let tokens = approx_token_count(body);
                request_tokens += tokens;
                if let Some((_, original)) = exact.get(*id)
                    && body.as_bytes() != original
                {
                    receipt_tokens += tokens;
                }
            }
            assert!(request_tokens <= budget, "{request_tokens} > {budget}");
            input_tokens += request_tokens;
            // Fixed follow-up needs old, middle and recent source. Recovery reads
            // stored bytes only; there is no producer invocation on this path.
            for index in [0, count / 2, count - 1] {
                let id = format!("completed-{index}");
                let (artifact, bytes) = &exact[&id];
                if visible
                    .get(id.as_str())
                    .is_none_or(|body| body.as_bytes() != bytes)
                {
                    recovery_calls += 1;
                    let recovered = read_exact_tool_output_artifact(home.path(), thread, artifact)
                        .await
                        .unwrap();
                    assert_eq!(&recovered, bytes);
                }
            }
        }
        assert!(receipt_tokens > 0);
        assert!(recovery_calls > 0);
        super::benchmarks::report(
            serde_json::json!({"case":format!("pressure-{budget}"),"budget":budget,"model_input_tool_tokens":input_tokens,"compact_receipt_tokens":receipt_tokens,"read_tool_output_recovery_calls":recovery_calls,"completion_ms":started.elapsed().as_secs_f64()*1000.0,"scope":"deterministic admission/recovery replay; no model latency or producer reruns"}),
        );
    }
    assert_eq!(
        model_visible_tool_result_token_budget_for_context_window(Some(258_000)),
        129_000
    );
}
