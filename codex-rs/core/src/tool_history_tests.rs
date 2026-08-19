use super::*;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::command_output_artifact::protect_active_tool_history_artifact;
use crate::tools::command_output_artifact::read_exact_tool_output_artifact;
use crate::tools::command_output_artifact::remint_tool_history_artifact_for_thread;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_tools::CanonicalToolResult;
use pretty_assertions::assert_eq;

fn bounded_output() -> String {
    "bounded model-visible tool output with enough material for a smaller receipt\n".repeat(700)
}

fn candidate(call_id: &str, bounded_model_output: String) -> ToolHistoryCandidate {
    ToolHistoryCandidate {
        call_id: call_id.to_string(),
        tool_identity: "functions.exec".to_string(),
        semantic_class: "tool_output".to_string(),
        source_dependencies: BTreeSet::new(),
        source_dependencies_current: true,
        artifact_id: "artifact-1".to_string(),
        artifact_bytes: 96_000,
        artifact_sha256: sha256(b"canonical artifact"),
        original_output_sha256: sha256(b"raw output before bounding"),
        original_tokens: 24_000,
        preserved_non_text_tokens: 0,
        bounded_model_output,
        complete: true,
        projection_eligible: true,
        proof_identity: None,
        supersession_identity: None,
        consumed_by_generation: None,
    }
}

fn text_output(call_id: &str, text: String) -> ResponseItem {
    ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_text(text),
        internal_chat_message_metadata_passthrough: None,
    }
}

fn function_call(call_id: &str) -> ResponseItem {
    named_function_call(call_id, "functions.exec")
}

fn named_function_call(call_id: &str, name: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: name.to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: call_id.to_string(),
        internal_chat_message_metadata_passthrough: None,
    }
}

#[test]
fn live_host_test_tool_observes_workspace() {
    assert!(tool_observes_workspace("exec_command"));
    assert!(tool_observes_workspace("cargo_test"));
    assert!(!tool_observes_workspace("exec"));
    assert!(!tool_observes_workspace("functions.exec"));
}

fn tool_search_pair(call_id: &str, description_bytes: usize) -> [ResponseItem; 2] {
    [
        ResponseItem::ToolSearchCall {
            id: None,
            call_id: Some(call_id.to_string()),
            status: Some("completed".to_string()),
            execution: "client".to_string(),
            arguments: serde_json::json!({"query": "example"}),
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::ToolSearchOutput {
            id: None,
            call_id: Some(call_id.to_string()),
            status: "completed".to_string(),
            execution: "client".to_string(),
            tools: vec![serde_json::json!({
                "name": format!("tool-{call_id}"),
                "description": "x".repeat(description_bytes),
            })],
            omitted_result_count: Some(0),
            internal_chat_message_metadata_passthrough: None,
        },
    ]
}

fn workspace_identity(label: &str) -> WorkspaceEvidenceIdentity {
    WorkspaceEvidenceIdentity {
        repository_root: Some(format!("/repo-{label}")),
        head_identity: Some(format!("head-{label}")),
        index_identity: Some(format!("index-{label}")),
        worktree_identity: Some(format!("worktree-{label}")),
    }
}

#[test]
fn workspace_evidence_captured_across_unobserved_revision_change_is_stale() {
    let call_id = "raced-call";
    let output = text_output(call_id, "old file contents".to_string());
    let captured = workspace_identity("after");
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item_with_freshness(
            Some(captured.clone()),
            &output,
            BTreeSet::new(),
            /*source_dependencies_current*/ false,
        )
        .expect("raced workspace observation"),
    );

    let projection = state.project_with_workspace_identity(canonical, Some(&captured));
    let (_, stale_output) = textual_output_identity(&projection.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn later_duplicate_registration_cannot_revive_invalidated_workspace_evidence() {
    let call_id = "raced-call";
    let output = text_output(call_id, "old file contents".to_string());
    let captured = workspace_identity("captured");
    let changed = workspace_identity("changed");
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(Some(captured), &output, BTreeSet::new())
            .expect("initial workspace observation"),
    );
    assert!(state.invalidate_source_dependencies(None, None));

    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(changed.clone()),
            &output,
            BTreeSet::new(),
        )
        .expect("later duplicate observation"),
    );

    let projection = state.project_with_workspace_identity(canonical, Some(&changed));
    let (_, stale_output) = textual_output_identity(&projection.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn workspace_evidence_remains_visible_only_for_its_captured_revision() {
    let call_id = "call-1";
    let output = text_output(call_id, "git status output".to_string());
    let captured = workspace_identity("captured");
    let changed = workspace_identity("changed");
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured.clone()),
            &output,
            BTreeSet::new(),
        )
        .expect("text evidence observation"),
    );

    let current = state.project_with_workspace_identity(Arc::clone(&canonical), Some(&captured));
    assert_eq!(current.items, canonical);

    let stale = state.project_with_workspace_identity(canonical, Some(&changed));
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
    assert_eq!(stale.unreplaced_items, stale.items);
}

#[test]
fn workspace_evidence_is_stale_in_a_different_repository() {
    let call_id = "call-1";
    let output = text_output(call_id, "git status output".to_string());
    let captured = workspace_identity("captured");
    let mut different_repository = captured.clone();
    different_repository.repository_root = Some("/other-repository".to_string());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(Some(captured), &output, BTreeSet::new())
            .expect("text evidence observation"),
    );

    let stale = state.project_with_workspace_identity(canonical, Some(&different_repository));
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn completed_command_evidence_uses_the_post_execution_revision() {
    let call_id = "mutating-call";
    let output = text_output(call_id, "mutation completed".to_string());
    let before = workspace_identity("before");
    let after = workspace_identity("after");
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(after.clone()),
            &output,
            BTreeSet::new(),
        )
        .expect("post-execution observation"),
    );

    let current = state.project_with_workspace_identity(Arc::clone(&canonical), Some(&after));
    assert_eq!(current.items, canonical);

    let stale = state.project_with_workspace_identity(canonical, Some(&before));
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn non_git_workspace_evidence_remains_visible_without_a_git_revision() {
    let call_id = "non-git-call";
    let output = text_output(call_id, "plain directory output".to_string());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(None, &output, BTreeSet::new())
            .expect("non-git observation"),
    );

    let current = state.project_with_workspace_identity(Arc::clone(&canonical), None);
    assert_eq!(current.items, canonical);

    let initialized =
        state.project_with_workspace_identity(canonical, Some(&workspace_identity("initialized")));
    let (_, stale_output) = textual_output_identity(&initialized.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn non_git_unknown_workspace_evidence_invalidates_on_recorded_mutation() {
    let call_id = "non-git-call";
    let output = text_output(call_id, "plain directory output".to_string());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(None, &output, BTreeSet::new())
            .expect("non-git observation"),
    );

    let unavailable = state.project_with_workspace_identity(Arc::clone(&canonical), None);
    let (_, unavailable_output) =
        textual_output_identity(&unavailable.items[1]).expect("unavailable output");
    assert!(unavailable_output.contains("\"stale_workspace_evidence\":true"));

    assert!(state.invalidate_source_dependencies(
        Some(&BTreeSet::from([PathBuf::from("/repo/changed.rs")])),
        None,
    ));
    let stale = state.project_with_workspace_identity(canonical, None);
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn untracked_legacy_workspace_evidence_fails_closed() {
    let call_id = "call-1";
    let canonical: Arc<[ResponseItem]> = Arc::from([
        function_call(call_id),
        text_output(call_id, "old test result".to_string()),
    ]);
    let projection = ToolHistoryState::default()
        .project_with_workspace_identity(canonical, Some(&workspace_identity("current")));
    let (_, output) = textual_output_identity(&projection.items[1]).expect("stale output");
    assert!(output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn recovered_workspace_evidence_inherits_origin_revision() {
    let origin_call_id = "origin-call";
    let recovery_call_id = "recovery-call";
    let origin_output = text_output(origin_call_id, "old file contents".to_string());
    let captured = workspace_identity("captured");
    let mut state = ToolHistoryState::default();
    state.register(candidate(origin_call_id, "old file contents".to_string()));
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured),
            &origin_output,
            BTreeSet::new(),
        )
        .expect("origin workspace observation"),
    );
    let canonical: Arc<[ResponseItem]> = Arc::from([
        ResponseItem::FunctionCall {
            id: None,
            name: "read_tool_output".to_string(),
            namespace: None,
            arguments: serde_json::json!({"artifact_id": "artifact-1"}).to_string(),
            call_id: recovery_call_id.to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
        text_output(recovery_call_id, "old file contents".to_string()),
    ]);

    let projection =
        state.project_with_workspace_identity(canonical, Some(&workspace_identity("changed")));
    let (_, output) = textual_output_identity(&projection.items[1]).expect("stale recovery");
    assert!(output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn workspace_evidence_invalidates_only_overlapping_source_dependencies() {
    let call_id = "call-1";
    let output = text_output(call_id, "search result".to_string());
    let canonical: Arc<[ResponseItem]> =
        Arc::from([named_function_call(call_id, "exec_command"), output.clone()]);
    let foo = PathBuf::from("/repo/src/foo.rs");
    let captured = workspace_identity("captured");
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured),
            &output,
            BTreeSet::from([SourceDependencyV1::new(&foo, false)]),
        )
        .expect("workspace observation"),
    );

    let after_unrelated = workspace_identity("after-unrelated");
    assert!(state.invalidate_source_dependencies(
        Some(&BTreeSet::from([PathBuf::from("/repo/src/bar.rs")])),
        Some(&after_unrelated),
    ));
    let unrelated =
        state.project_with_workspace_identity(Arc::clone(&canonical), Some(&after_unrelated));
    assert_eq!(unrelated.items, canonical);

    assert!(state.invalidate_source_dependencies(
        Some(&BTreeSet::from([foo])),
        Some(&workspace_identity("after-overlap")),
    ));
    let stale = state.project_with_workspace_identity(canonical, Some(&after_unrelated));
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("a source dependency changed"));
}

#[test]
fn watcher_proof_retains_dependency_scoped_evidence_after_external_disjoint_edit() {
    let root = tempfile::tempdir().expect("workspace root");
    let source = root.path().join("src/foo.rs");
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("create source dir");
    std::fs::write(&source, "fn foo() {}\n").expect("write source");
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let path_observation = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .expect("source path observation");
    let call_id = "call-1";
    let output = text_output(call_id, "search result".to_string());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut captured = workspace_identity("captured");
    captured.repository_root = Some(root.path().to_string_lossy().into_owned());
    let mut changed = workspace_identity("changed");
    changed.repository_root = captured.repository_root.clone();
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured),
            &output,
            BTreeSet::from([SourceDependencyV1::new(&source, false)]),
        )
        .expect("workspace observation")
        .with_source_path_observations(vec![path_observation]),
    );

    cache.note_host_workspace_mutation_paths(root.path(), &["README.md".to_string()]);
    let unrelated =
        state.project_with_workspace_cache(Arc::clone(&canonical), Some(&changed), cache.as_ref());
    assert_eq!(unrelated.items, canonical);

    cache.note_host_workspace_mutation_paths(root.path(), &["src/foo.rs".to_string()]);
    let stale = state.project_with_workspace_cache(canonical, Some(&changed), cache.as_ref());
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("stale_workspace_evidence"));
}

#[test]
fn dependency_scoped_workspace_evidence_invalidates_on_external_revision_change() {
    let call_id = "call-1";
    let output = text_output(call_id, "search result".to_string());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(workspace_identity("captured")),
            &output,
            BTreeSet::from([SourceDependencyV1::new(
                Path::new("/repo/src/foo.rs"),
                false,
            )]),
        )
        .expect("workspace observation"),
    );

    let stale = state
        .project_with_workspace_identity(canonical, Some(&workspace_identity("external-edit")));
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
}

#[test]
fn command_dependencies_cover_search_test_and_ownership_inputs() {
    let cwd = Path::new("/repo");
    let search = ToolPayload::Function {
        arguments: serde_json::json!({
            "program": "rg",
            "args": ["needle", "src/foo.rs"],
            "workdir": "/repo"
        })
        .to_string(),
    };
    assert_eq!(
        source_dependencies_for_tool_call("exec_command", &search, cwd),
        BTreeSet::from([SourceDependencyV1::new(
            Path::new("/repo/src/foo.rs"),
            false
        )])
    );

    let test = ToolPayload::Function {
        arguments: serde_json::json!({"command": ["cargo", "test"]}).to_string(),
    };
    assert_eq!(
        source_dependencies_for_tool_call("exec_command", &test, cwd),
        BTreeSet::from([SourceDependencyV1::new(cwd, true)])
    );

    let python_test = ToolPayload::Function {
        arguments: serde_json::json!({"command": ["python", "-m", "pytest"]}).to_string(),
    };
    assert_eq!(
        source_dependencies_for_tool_call("exec_command", &python_test, cwd),
        BTreeSet::from([SourceDependencyV1::new(cwd, true)])
    );

    let read = ToolPayload::Function {
        arguments: serde_json::json!({"command": ["cat", "src/foo.rs"]}).to_string(),
    };
    assert_eq!(
        source_dependencies_for_tool_call("exec_command", &read, cwd),
        BTreeSet::from([SourceDependencyV1::new(
            Path::new("/repo/src/foo.rs"),
            false,
        )])
    );

    let ownership = ToolPayload::Function {
        arguments: serde_json::json!({
            "program": "python",
            "args": ["scripts/source_owners.py", "check"]
        })
        .to_string(),
    };
    let ownership_dependencies = source_dependencies_for_tool_call("exec_command", &ownership, cwd);
    assert!(ownership_dependencies.contains(&SourceDependencyV1::new(
        Path::new("/repo/source_owners.toml"),
        false,
    )));
    assert!(ownership_dependencies.contains(&SourceDependencyV1::new(
        Path::new("/repo/architecture_index.json"),
        false,
    )));
}

#[test]
fn cargo_test_dependencies_follow_selected_local_package_graph() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\", \"support\"]\nresolver = \"2\"\n",
    )
    .expect("workspace manifest");
    std::fs::create_dir_all(temp.path().join("app/src")).expect("app source");
    std::fs::create_dir_all(temp.path().join("support/src")).expect("support source");
    std::fs::write(
        temp.path().join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dev-dependencies]\nsupport = { path = \"../support\" }\n",
    )
    .expect("app manifest");
    std::fs::write(
        temp.path().join("support/Cargo.toml"),
        "[package]\nname = \"support\"\nversion = \"0.1.0\"\n",
    )
    .expect("support manifest");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({"package": "app", "workdir": temp.path()}).to_string(),
    };
    let dependencies = source_dependencies_for_tool_call("cargo_test", &payload, temp.path());
    assert!(dependencies.contains(&SourceDependencyV1::new(&temp.path().join("app"), true,)));
    assert!(dependencies.contains(&SourceDependencyV1::new(&temp.path().join("support"), true,)));
    assert!(!dependencies.contains(&SourceDependencyV1::new(temp.path(), true)));
}

#[test]
fn workspace_evidence_cwd_uses_explicit_workdir() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "program": "git",
            "args": ["status", "--short"],
            "workdir": "nested/repository"
        })
        .to_string(),
    };
    assert_eq!(
        workspace_evidence_cwd_for_tool_call("exec_command", &payload, Path::new("/workspace"),),
        PathBuf::from("/workspace/nested/repository")
    );
}

#[test]
fn mutation_boundary_repository_history_reads_and_writers_skip_full_workspace_evidence() {
    for args in [
        serde_json::json!(["-C", ".", "log", "-1"]),
        serde_json::json!(["-C", ".", "add", "src/lib.rs"]),
    ] {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"program": "git", "args": args}).to_string(),
        };
        assert!(!tool_call_observes_workspace("exec_command", &payload));
    }

    let payload = ToolPayload::Function {
        arguments: serde_json::json!({"program": "rg", "args": ["needle", "src"]}).to_string(),
    };
    assert!(tool_call_observes_workspace("exec_command", &payload));
}

async fn stored_candidate(
    codex_home: &std::path::Path,
    thread_id: &str,
    call_id: &str,
    bounded_model_output: String,
) -> (ToolHistoryCandidate, Vec<u8>) {
    let canonical_bytes = bounded_model_output.as_bytes().to_vec();
    let canonical = CanonicalToolResult::bytes(canonical_bytes.clone());
    let artifact = create_canonical_output_artifact(codex_home, thread_id, &canonical).await;
    assert!(artifact.complete);
    let artifact_id = artifact.artifact_id().expect("stored artifact id");
    protect_active_tool_history_artifact(
        codex_home,
        thread_id,
        &artifact_id,
        canonical.exact_bytes,
        &canonical.sha256,
    )
    .await
    .expect("protect source artifact");
    let mut candidate = candidate(call_id, bounded_model_output);
    candidate.artifact_id = artifact_id;
    candidate.artifact_bytes = canonical.exact_bytes;
    candidate.artifact_sha256 = canonical.sha256;
    (candidate, canonical_bytes)
}

#[test]
fn completed_tool_history_receipt_lifecycle_keeps_canonical_history_unchanged() {
    let call_id = "call-1";
    let bounded = bounded_output();
    let canonical: Arc<[ResponseItem]> = Arc::from([text_output(call_id, bounded.clone())]);
    let canonical_before = serde_json::to_vec(&canonical).expect("serialize canonical history");
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, bounded));

    assert!(state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));
    let projection = state.project(Arc::clone(&canonical));

    assert!(projection.unreplaced_items.is_empty());
    assert_eq!(projection.substitutions.len(), 1);
    assert_eq!(projection.substitutions[0].item_index, 0);
    assert_eq!(projection.substitutions[0].call_id, call_id);
    assert!(response_item_has_valid_tool_history_receipt(
        &projection.items[0]
    ));
    assert_eq!(
        serde_json::to_vec(&canonical).expect("serialize canonical history"),
        canonical_before
    );
}

#[test]
fn completed_tool_history_receipt_does_not_expose_source_dependency_paths() {
    let call_id = "call-1";
    let bounded = bounded_output();
    let canonical: Arc<[ResponseItem]> = Arc::from([text_output(call_id, bounded.clone())]);
    let mut tracked = candidate(call_id, bounded);
    tracked.source_dependencies = BTreeSet::from([SourceDependencyV1::new(
        Path::new("/private/workspace/src/secret.rs"),
        false,
    )]);
    let mut state = ToolHistoryState::default();
    state.register(tracked);
    assert!(state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));

    let projection = state.project(canonical);
    let (_, receipt) = textual_output_identity(&projection.items[0]).expect("receipt output");
    assert!(!receipt.contains("/private/workspace"));
    assert!(serde_json::from_str::<ToolHistoryReceiptV1>(receipt).is_ok());
}

#[test]
fn tool_history_receipt_requires_consumed_complete_matching_bounded_output() {
    let call_id = "call-1";
    let bounded = "small bounded output".to_string();
    let canonical: Arc<[ResponseItem]> = Arc::from([text_output(call_id, bounded.clone())]);
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, bounded.clone()));

    assert!(
        state
            .project(Arc::clone(&canonical))
            .substitutions
            .is_empty()
    );
    assert!(!state.mark_consumed(
        &[text_output(call_id, "tampered".to_string())],
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));

    let mut incomplete = candidate(call_id, bounded);
    incomplete.complete = false;
    state.register(incomplete);
    assert!(state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 2,
        },
    ));
    assert!(state.project(canonical).substitutions.is_empty());
}

#[test]
fn tool_history_admission_bounds_aggregate_first_exposure_and_consumes_receipts() {
    let first = "alpha ".repeat(6_000);
    let second = "beta ".repeat(6_000);
    let canonical: Arc<[ResponseItem]> = Arc::from([
        text_output("call-1", first.clone()),
        text_output("call-2", second.clone()),
    ]);
    let mut state = ToolHistoryState::default();
    state.register(candidate("call-1", first));
    state.register(candidate("call-2", second));

    let projection = state.project(Arc::clone(&canonical));
    assert_eq!(projection.unreplaced_items.len(), 1);
    assert!(
        projection
            .items
            .iter()
            .filter_map(textual_output_identity)
            .map(|(_, output)| approx_token_count(output))
            .sum::<usize>()
            <= MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET
    );
    assert_eq!(projection.substitutions.len(), 1);
    assert_eq!(
        projection
            .items
            .iter()
            .filter(|item| response_item_has_valid_tool_history_receipt(item))
            .count(),
        1
    );
    assert!(state.mark_consumed(
        &projection.items,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));
    assert_eq!(state.consumed_outputs_for_tool("functions.exec").len(), 2);
    assert_eq!(state.project(canonical).substitutions.len(), 2);
}

#[test]
fn tool_history_admission_charges_receipts_and_drops_unrepresentable_pairs() {
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for index in 0..80 {
        let call_id = format!("call-{index}");
        let output = format!("result-{index} ").repeat(20_000);
        state.register(candidate(&call_id, output.clone()));
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, output));
    }

    let projection = state.project(Arc::from(items));
    let projected_tokens = projection
        .items
        .iter()
        .filter_map(textual_output_identity)
        .map(|(_, output)| approx_token_count(output))
        .sum::<usize>();

    assert!(projected_tokens <= MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET);
    assert!(projection.items.len() < 160);
    assert!(projection.unreplaced_items.is_empty());
    assert!(
        projection
            .items
            .iter()
            .filter_map(textual_output_identity)
            .all(|(_, output)| serde_json::from_str::<ToolHistoryReceiptV1>(output).is_ok())
    );
}

#[test]
fn tool_history_admission_charges_preserved_non_text_content() {
    let call_id = "image-call";
    let output = "small text".to_string();
    let mut image_candidate = candidate(call_id, output.clone());
    image_candidate.preserved_non_text_tokens =
        MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET.saturating_add(1) as u64;
    let mut state = ToolHistoryState::default();
    state.register(image_candidate);

    let projection = state.project(Arc::from([
        function_call(call_id),
        text_output(call_id, output),
    ]));

    assert!(projection.items.is_empty());
    assert!(projection.unreplaced_items.is_empty());
}

#[test]
fn tool_history_admission_prioritizes_plain_failure_outputs() {
    let failure = "failure evidence ".repeat(1_800);
    let success = "ordinary success ".repeat(1_800);
    let mut failure_candidate = candidate("failure-call", failure.clone());
    failure_candidate.semantic_class = "tool_failure".to_string();
    let mut state = ToolHistoryState::default();
    state.register(failure_candidate);
    state.register(candidate("success-call", success.clone()));

    let projection = state.project(Arc::from([
        function_call("failure-call"),
        text_output("failure-call", failure.clone()),
        function_call("success-call"),
        text_output("success-call", success),
    ]));

    assert!(projection.items.iter().any(|item| {
        textual_output_identity(item)
            .is_some_and(|(call_id, output)| call_id == "failure-call" && output == failure)
    }));
    assert!(projection.items.iter().any(|item| {
        textual_output_identity(item).is_some_and(|(call_id, output)| {
            call_id == "success-call"
                && serde_json::from_str::<ToolHistoryReceiptV1>(output).is_ok()
        })
    }));
}

#[test]
fn tool_history_admission_budgets_structured_tool_search_pairs() {
    let older = tool_search_pair("search-older", 24_000);
    let latest = tool_search_pair("search-latest", 24_000);
    let canonical: Arc<[ResponseItem]> = Arc::from([
        older[0].clone(),
        older[1].clone(),
        latest[0].clone(),
        latest[1].clone(),
    ]);

    let projection = ToolHistoryState::default().project(canonical);

    assert_eq!(projection.items.len(), 2);
    assert!(
        projection
            .items
            .iter()
            .all(|item| item_call_id(item) == Some("search-latest"))
    );
    assert_eq!(projection.items, projection.unreplaced_items);
}

#[test]
fn tool_history_admission_supersedes_identical_results_with_latest_pair() {
    let bounded = bounded_output();
    let mut first = candidate("call-1", bounded.clone());
    first.supersession_identity = Some(format!(
        "functions.exec:{}:{}",
        sha256(b"same action"),
        sha256(b"same result")
    ));
    let mut second = candidate("call-2", bounded.clone());
    second.supersession_identity = first.supersession_identity.clone();
    let canonical: Arc<[ResponseItem]> = Arc::from([
        function_call("call-1"),
        text_output("call-1", bounded.clone()),
        function_call("call-2"),
        text_output("call-2", bounded),
    ]);
    let mut state = ToolHistoryState::default();
    state.register(first);
    state.register(second);

    let projection = state.project(Arc::clone(&canonical));
    assert_eq!(canonical.len(), 4);
    assert!(projection.unreplaced_items.is_empty());
    assert_eq!(projection.items.len(), 2);
    assert!(
        projection
            .items
            .iter()
            .all(|item| { item_call_id(item).is_some_and(|call_id| call_id == "call-2") })
    );
}

#[test]
fn legacy_result_only_supersession_identity_does_not_collapse_actions() {
    let bounded = bounded_output();
    let mut first = candidate("call-1", bounded.clone());
    first.supersession_identity = Some(format!("functions.exec:{}", sha256(b"same result")));
    let mut second = candidate("call-2", bounded.clone());
    second.supersession_identity = first.supersession_identity.clone();
    let canonical: Arc<[ResponseItem]> = Arc::from([
        function_call("call-1"),
        text_output("call-1", bounded.clone()),
        function_call("call-2"),
        text_output("call-2", bounded),
    ]);
    let mut state = ToolHistoryState::default();
    state.register(first);
    state.register(second);

    let projection = state.project(canonical);

    assert_eq!(projection.items.len(), 4);
    assert!(
        projection
            .items
            .iter()
            .any(|item| item_call_id(item) == Some("call-1"))
    );
    assert!(
        projection
            .items
            .iter()
            .any(|item| item_call_id(item) == Some("call-2"))
    );
}

#[test]
fn mcp_content_item_receipt_preserves_non_text_modalities() {
    let call_id = "mcp-call";
    let bounded = bounded_output();
    let canonical: Arc<[ResponseItem]> = Arc::from([ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText {
                text: bounded.clone(),
            },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,aW1hZ2U=".to_string(),
                detail: None,
            },
            FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "opaque".to_string(),
            },
        ]),
        internal_chat_message_metadata_passthrough: None,
    }]);
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, bounded));
    assert!(state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));

    let projection = state.project(canonical);
    let ResponseItem::FunctionCallOutput { output, .. } = &projection.items[0] else {
        panic!("expected function output");
    };
    let FunctionCallOutputBody::ContentItems(items) = &output.body else {
        panic!("expected MCP content items");
    };
    assert!(matches!(
        &items[0],
        FunctionCallOutputContentItem::InputText { text }
            if serde_json::from_str::<ToolHistoryReceiptV1>(text).is_ok()
    ));
    assert!(matches!(
        &items[1],
        FunctionCallOutputContentItem::InputImage { image_url, .. }
            if image_url == "data:image/png;base64,aW1hZ2U="
    ));
    assert!(matches!(
        &items[2],
        FunctionCallOutputContentItem::EncryptedContent { encrypted_content }
            if encrypted_content == "opaque"
    ));
}

#[test]
fn structural_receipt_validation_rejects_receipt_like_text_and_tampering() {
    let call_id = "call-1";
    let bounded = bounded_output();
    let canonical: Arc<[ResponseItem]> = Arc::from([text_output(call_id, bounded.clone())]);
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, bounded));
    state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    );
    let projection = state.project(canonical);
    let ResponseItem::FunctionCallOutput { output, .. } = &projection.items[0] else {
        panic!("expected receipt output");
    };
    let FunctionCallOutputBody::Text(receipt) = &output.body else {
        panic!("expected text receipt");
    };
    let mut tampered: serde_json::Value =
        serde_json::from_str(receipt).expect("valid receipt JSON");
    tampered["receipt_id"] = serde_json::Value::String("thr1-tampered".to_string());

    assert!(!response_item_has_valid_tool_history_receipt(&text_output(
        call_id,
        "receipt_id artifact sha256 complete".to_string(),
    )));
    assert!(!response_item_has_valid_tool_history_receipt(&text_output(
        call_id,
        serde_json::to_string(&tampered).expect("serialize tampered receipt"),
    )));
}

#[test]
fn legacy_tool_history_ledger_keys_remain_compatible() {
    let bounded = bounded_output();
    let state = ToolHistoryState {
        candidates: BTreeMap::from([("call-1".to_string(), candidate("call-1", bounded))]),
        workspace_evidence: BTreeMap::new(),
    };
    let mut serialized = serde_json::to_value(&state).expect("serialize ledger state");
    let candidate = &serialized["candidates"]["call-1"];
    assert!(candidate.get("bounded_digest").is_some());
    assert!(candidate.get("bounded_model_output").is_none());
    serialized["provider_authoritative_outputs"] = serde_json::json!({"call-1": "legacy"});
    serialized["provider_baseline"] = serde_json::json!({"mode": "incremental"});

    let restored: ToolHistoryState =
        serde_json::from_value(serialized).expect("legacy fields should be ignored");
    assert!(restored.candidates.contains_key("call-1"));
}

#[tokio::test]
async fn fork_remints_receipt_artifact_into_child_namespace() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_thread_id = "source-thread";
    let target_thread_id = "target-thread";
    let call_id = "call-1";
    let bounded = bounded_output();
    let (candidate, canonical_bytes) =
        stored_candidate(temp.path(), source_thread_id, call_id, bounded.clone()).await;
    let source_artifact_id = candidate.artifact_id.clone();
    let canonical_history: Arc<[ResponseItem]> = Arc::from([text_output(call_id, bounded.clone())]);
    let mut source_state = ToolHistoryState::default();
    source_state.register(candidate);
    assert!(source_state.mark_consumed(
        &canonical_history,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));
    persist_tool_history_state(temp.path(), source_thread_id, &source_state)
        .await
        .expect("persist source ledger");

    let loaded = load_tool_history_state_for_fork(temp.path(), source_thread_id).await;
    let (forked, dropped) =
        remint_tool_history_state_for_fork(temp.path(), source_thread_id, target_thread_id, loaded)
            .await;
    assert_eq!(dropped, 0);
    let target_artifact_id = forked.candidates[call_id].artifact_id.clone();
    assert_eq!(target_artifact_id, source_artifact_id);

    let forked = reconcile_tool_history_state(temp.path(), target_thread_id, forked).await;
    persist_tool_history_state(temp.path(), target_thread_id, &forked)
        .await
        .expect("persist target ledger");
    let restored = load_tool_history_state(temp.path(), target_thread_id).await;
    let projection = restored.project(canonical_history);
    assert_eq!(projection.substitutions.len(), 1);
    let ResponseItem::FunctionCallOutput { output, .. } = &projection.items[0] else {
        panic!("expected projected function output");
    };
    let FunctionCallOutputBody::Text(receipt) = &output.body else {
        panic!("expected receipt text");
    };
    let receipt: ToolHistoryReceiptV1 = serde_json::from_str(receipt).expect("receipt JSON");
    assert_eq!(receipt.artifact.artifact_id, target_artifact_id);
    assert_eq!(
        read_exact_tool_output_artifact(temp.path(), target_thread_id, &target_artifact_id)
            .await
            .expect("read reminted artifact"),
        canonical_bytes
    );
    assert_eq!(
        read_exact_tool_output_artifact(temp.path(), source_thread_id, &source_artifact_id)
            .await
            .expect("read source artifact"),
        canonical_bytes
    );
    assert_ne!(
        temp.path()
            .join("tool-output")
            .join(source_thread_id)
            .join(format!("{source_artifact_id}.log")),
        temp.path()
            .join("tool-output")
            .join(target_thread_id)
            .join(format!("{target_artifact_id}.log"))
    );
}

#[tokio::test]
async fn reconciliation_keeps_unconsumed_artifacts_and_releases_pruned_history() {
    let temp = tempfile::tempdir().expect("tempdir");
    let thread_id = "thread";
    let call_id = "call-1";
    let (candidate, _) = stored_candidate(temp.path(), thread_id, call_id, bounded_output()).await;
    let mut state = ToolHistoryState::default();
    state.register(candidate);
    persist_tool_history_state(temp.path(), thread_id, &state)
        .await
        .expect("persist ledger");

    let mut restored = load_tool_history_state(temp.path(), thread_id).await;
    assert!(restored.candidates.contains_key(call_id));
    restored.retain_for_history(&[]);
    let reconciled = reconcile_tool_history_state(temp.path(), thread_id, restored).await;
    persist_tool_history_state(temp.path(), thread_id, &reconciled)
        .await
        .expect("persist pruned ledger");
    assert!(
        load_tool_history_state(temp.path(), thread_id)
            .await
            .candidates
            .is_empty()
    );

    let mut entries = tokio::fs::read_dir(temp.path().join("tool-output").join(thread_id))
        .await
        .expect("artifact directory");
    while let Some(entry) = entries.next_entry().await.expect("directory entry") {
        assert_ne!(
            entry
                .path()
                .extension()
                .and_then(|extension| extension.to_str()),
            Some("active-tool-history")
        );
    }
}

#[tokio::test]
async fn fork_artifact_remint_is_idempotent_and_never_overwrites_a_collision() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source_thread_id = "source-thread";
    let target_thread_id = "target-thread";
    let (candidate, canonical_bytes) =
        stored_candidate(temp.path(), source_thread_id, "call-1", bounded_output()).await;

    let reminted_id = remint_tool_history_artifact_for_thread(
        temp.path(),
        source_thread_id,
        target_thread_id,
        &candidate.artifact_id,
        candidate.artifact_bytes,
        &candidate.artifact_sha256,
    )
    .await
    .expect("initial remint");
    assert_eq!(reminted_id, candidate.artifact_id);
    assert_eq!(
        remint_tool_history_artifact_for_thread(
            temp.path(),
            source_thread_id,
            target_thread_id,
            &candidate.artifact_id,
            candidate.artifact_bytes,
            &candidate.artifact_sha256,
        )
        .await
        .expect("idempotent remint"),
        candidate.artifact_id
    );

    let colliding_thread_id = "colliding-target-thread";
    let colliding_directory = temp.path().join("tool-output").join(colliding_thread_id);
    tokio::fs::create_dir_all(&colliding_directory)
        .await
        .expect("collision directory");
    let colliding_path = colliding_directory.join(format!("{}.log", candidate.artifact_id));
    let colliding_bytes = b"unrelated existing artifact".to_vec();
    tokio::fs::write(&colliding_path, &colliding_bytes)
        .await
        .expect("collision artifact");
    assert!(
        remint_tool_history_artifact_for_thread(
            temp.path(),
            source_thread_id,
            colliding_thread_id,
            &candidate.artifact_id,
            candidate.artifact_bytes,
            &candidate.artifact_sha256,
        )
        .await
        .is_err()
    );
    assert_eq!(
        tokio::fs::read(colliding_path)
            .await
            .expect("collision artifact remains"),
        colliding_bytes
    );
    assert_eq!(
        read_exact_tool_output_artifact(temp.path(), target_thread_id, &reminted_id)
            .await
            .expect("read reminted artifact"),
        canonical_bytes
    );
}
