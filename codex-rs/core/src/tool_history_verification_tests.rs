use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn delayed_external_events_cannot_leave_sampling_evidence_unqualified() {
    let root = tempfile::tempdir().unwrap();
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let revision = workspace_identity_at(root.path(), "unchanged-journal");
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::write(&a, "0").unwrap();
    std::fs::write(&b, "0").unwrap();
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for (call, path, value) in [("a", &a, "0"), ("b", &b, "1")] {
        if call == "b" {
            // No events are delivered by this watcher. A=0/B=1 never coexisted.
            std::fs::write(&a, "1").unwrap();
            std::fs::write(&b, "1").unwrap();
        }
        assert_eq!(std::fs::read_to_string(path).unwrap(), value);
        let arguments = serde_json::json!({"path":path});
        let classification = classify_workspace_tool_call(
            "read_file",
            &ToolPayload::Function {
                arguments: arguments.to_string(),
            },
            root.path(),
        );
        let output = text_output(call, value.into());
        state.register_workspace_evidence(
            watched(
                &cache,
                root.path(),
                WorkspaceEvidenceObservation::from_response_item_with_freshness(
                    Some(revision.clone()),
                    &output,
                    classification.source_dependencies,
                    true,
                )
                .unwrap()
                .with_source_scope_complete(true),
            )
            .await,
        );
        items.push(named_function_call_with_arguments(
            call,
            "read_file",
            arguments,
        ));
        items.push(output);
    }
    // Persistence/restart must not promote older watcher-only observations.
    let state: ToolHistoryState =
        serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
    let items: Arc<[ResponseItem]> = items.into();
    let projected = state
        .project_workspace_freshness_with_cache(Arc::clone(&items), Some(&revision), &cache)
        .items;
    assert!(projected.starts_with(&items));
    let qualification = projected
        .iter()
        .find_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "developer" => {
                content.iter().find_map(|part| match part {
                    codex_protocol::models::ContentItem::InputText { text }
                        if text.starts_with("<workspace_source_verification>") =>
                    {
                        text.lines()
                            .find_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .expect("sampling must qualify watcher-only evidence even without delivered events");
    assert_eq!(
        qualification["source_verification"]["level"],
        "watcher_observed"
    );
    assert_eq!(
        qualification["source_verification"]["current_source_verified"],
        false
    );
    assert_eq!(
        qualification["source_verification"]["coherent_snapshot_verified"],
        false
    );
    let repeated = state
        .project_workspace_freshness_with_cache(Arc::clone(&projected), Some(&revision), &cache)
        .items;
    assert_eq!(
        repeated, projected,
        "unchanged sampling must not duplicate qualifications"
    );
    cache
        .note_host_workspace_mutation_paths(root.path(), &[a.to_string_lossy().into_owned()])
        .await;
    assert!(
        !freshness_notices(&state, &repeated, Some(&revision), &cache).is_empty(),
        "qualification must not conceal later actual invalidation"
    );
}
