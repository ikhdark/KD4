use super::*;

fn request(source: &str) -> codex_code_mode::ExecuteRequest {
    codex_code_mode::ExecuteRequest {
        state_path: None,
        tool_call_id: "namespace-fixture".into(),
        enabled_tools: Vec::new().into(),
        source: source.into(),
        yield_time_ms: Some(60_000),
        max_output_tokens: None,
        default_tool_timeout_ms: None,
    }
}

#[tokio::test]
async fn foreign_cell_handles_cannot_observe_or_terminate_another_session() {
    let provider: Arc<dyn CodeModeSessionProvider> =
        Arc::new(codex_code_mode::InProcessCodeModeSessionProvider);
    let parent = CodeModeService::new(Arc::clone(&provider));
    let child = CodeModeService::new(provider);
    let parent_cell = parent.execute(request("await yield_control(); await new Promise(resolve => setTimeout(resolve, 3_600_000));")).await.unwrap();
    let parent_id = parent_cell.cell_id.clone();
    assert!(matches!(parent_cell.initial_response().await.unwrap(), RuntimeResponse::ExplicitYield { .. }));
    let child_cell = child.execute(request("await yield_control(); await new Promise(resolve => setTimeout(resolve, 3_600_000));")).await.unwrap();
    let child_id = child_cell.cell_id.clone();
    assert!(matches!(child_cell.initial_response().await.unwrap(), RuntimeResponse::ExplicitYield { .. }));
    // A fork can retain exactly this parent handle in copied conversation text.
    let foreign_wait = tokio::time::timeout(Duration::from_secs(2), child.wait_for_decision(parent_id.clone()))
        .await.expect("foreign wait must not attach to the child's suspended cell");
    let foreign_terminate = child.terminate(parent_id).await;
    let own_terminate = child.terminate(child_id).await;
    parent.shutdown().await.unwrap();
    child.shutdown().await.unwrap();
    assert!(matches!(foreign_wait, Err(_) | Ok(codex_code_mode::WaitOutcome::MissingCell(_))),
        "foreign wait must not resume the child's cell: {foreign_wait:?}");
    assert!(matches!(foreign_terminate, Err(_) | Ok(codex_code_mode::WaitOutcome::MissingCell(_))),
        "foreign termination must not affect the child's cell");
    assert!(matches!(own_terminate.unwrap(), codex_code_mode::WaitOutcome::LiveCell(RuntimeResponse::Terminated { .. })));
}
