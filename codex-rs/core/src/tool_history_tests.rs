use super::*;
#[test]
fn source_dependency_normalization_preserves_case_on_case_sensitive_filesystems() {
    let upper = normalized_source_path_with_case_sensitivity(Path::new("src/Owner.rs"), true);
    let lower = normalized_source_path_with_case_sensitivity(Path::new("src/owner.rs"), true);

    assert_ne!(upper, lower);
    assert_eq!(
        normalized_source_path_with_case_sensitivity(Path::new("src/Owner.rs"), false),
        normalized_source_path_with_case_sensitivity(Path::new("src/owner.rs"), false),
    );
}

#[test]
fn identity_projection_reuses_shared_response_items() {
    let canonical: Arc<[ResponseItem]> = Arc::from([ResponseItem::FunctionCall {
        id: None,
        name: "non_workspace_operation".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "identity-call".to_string(),
        internal_chat_message_metadata_passthrough: None,
    }]);

    let projection =
        ToolHistoryState::default().project_with_workspace_identity(Arc::clone(&canonical), None);

    assert!(Arc::ptr_eq(&projection.items, &canonical));
    assert!(Arc::ptr_eq(&projection.unreplaced_items, &canonical));
}
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
    let mut candidate = ToolHistoryCandidate {
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
        derived: ToolHistoryCandidateDerived::default(),
    };
    candidate.refresh_derived();
    candidate
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

fn expect_loaded_tool_history(outcome: ToolHistoryLoadOutcome) -> ToolHistoryState {
    match outcome {
        ToolHistoryLoadOutcome::Loaded(state) => state,
        outcome => panic!("expected loaded tool-history state, got {outcome:?}"),
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

    let current = state.project_with_workspace_identity(Arc::clone(&canonical), None);
    assert_eq!(current.items, canonical);

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
fn generation_batch_invalidation_excludes_its_own_completed_calls() {
    let current_call_id = "current-generation-call";
    let older_call_id = "older-call";
    let dependency = PathBuf::from("/repo/src/foo.rs");
    let captured = workspace_identity("captured");
    let current_output = text_output(current_call_id, "current result".to_string());
    let older_output = text_output(older_call_id, "older result".to_string());
    let source_dependencies = BTreeSet::from([SourceDependencyV1::new(&dependency, false)]);
    let mut state = ToolHistoryState::default();

    for (call_id, output) in [
        (current_call_id, &current_output),
        (older_call_id, &older_output),
    ] {
        let mut registered = candidate(
            call_id,
            textual_output_identity(output)
                .expect("textual output")
                .1
                .to_string(),
        );
        registered.tool_identity = "exec_command".to_string();
        registered.source_dependencies = source_dependencies.clone();
        registered.refresh_derived();
        state.register(registered);
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(
                Some(captured.clone()),
                output,
                source_dependencies.clone(),
            )
            .expect("workspace observation"),
        );
    }

    assert!(state.invalidate_source_dependencies_excluding_call_ids(
        Some(&BTreeSet::from([dependency])),
        Some(&workspace_identity("after-mutation")),
        &BTreeSet::from([current_call_id.to_string()]),
    ));

    assert!(state.candidates[current_call_id].source_dependencies_current);
    assert!(!state.candidates[older_call_id].source_dependencies_current);
    assert!(state.workspace_evidence[current_call_id].source_dependencies_current);
    assert!(!state.workspace_evidence[older_call_id].source_dependencies_current);
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
fn rg_dependencies_reuse_search_scope_parsing_for_options_and_compound_commands() {
    let cwd = Path::new("/repo");
    let explicit_pattern = ToolPayload::Function {
        arguments: serde_json::json!({
            "program": "rg",
            "args": ["--max-depth", "3", "-e", "needle", "codex-rs/core"],
            "workdir": "/repo"
        })
        .to_string(),
    };
    assert_eq!(
        source_dependencies_for_tool_call("exec_command", &explicit_pattern, cwd),
        BTreeSet::from([SourceDependencyV1::new(
            Path::new("/repo/codex-rs/core"),
            true,
        )])
    );

    let compound = ToolPayload::Function {
        arguments: serde_json::json!({
            "cmd": "Write-Output ready; rg -e needle codex-rs/tui",
            "shell": "powershell",
            "workdir": "/repo"
        })
        .to_string(),
    };
    assert_eq!(
        source_dependencies_for_tool_call("exec_command", &compound, cwd),
        BTreeSet::from([SourceDependencyV1::new(
            Path::new("/repo/codex-rs/tui"),
            true,
        )])
    );
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
    let package_index = cargo_package_index(temp.path());
    assert!(
        dependencies.contains(&SourceDependencyV1::new(&temp.path().join("app"), true,)),
        "selected package graph: {dependencies:#?}; package index: {package_index:#?}"
    );
    assert!(dependencies.contains(&SourceDependencyV1::new(&temp.path().join("support"), true,)));
    assert!(!dependencies.contains(&SourceDependencyV1::new(temp.path(), true)));
}

#[test]
fn duplicate_repository_reads_cargo_index_reuses_each_manifest() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root_manifest = "[workspace]\nmembers = [\"app\", \"support\"]\nresolver = \"2\"\n";
    std::fs::write(temp.path().join("Cargo.toml"), root_manifest).expect("workspace manifest");
    std::fs::create_dir_all(temp.path().join("app/src")).expect("app source");
    std::fs::create_dir_all(temp.path().join("support/src")).expect("support source");
    std::fs::write(
        temp.path().join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[dependencies]\nsupport = { path = \"../support\" }\n",
    )
    .expect("app manifest");
    std::fs::write(
        temp.path().join("support/Cargo.toml"),
        "[package]\nname = \"support\"\nversion = \"0.1.0\"\n",
    )
    .expect("support manifest");
    let mut reads = BTreeMap::<PathBuf, usize>::new();

    let mut index = cargo_package_index_with_manifest_reader(
        temp.path(),
        Some(root_manifest.to_string()),
        |path| {
            *reads.entry(path.to_path_buf()).or_default() += 1;
            std::fs::read_to_string(path).ok()
        },
    );

    assert_eq!(reads.get(&temp.path().join("Cargo.toml")), None);
    assert_eq!(reads.get(&temp.path().join("app/Cargo.toml")), Some(&1));
    assert_eq!(reads.get(&temp.path().join("support/Cargo.toml")), Some(&1));

    let app_root = index.packages.get("app").expect("app package").clone();
    index
        .manifests
        .get_mut(&app_root)
        .expect("canonical app manifest")
        .source = "this source is no longer parseable TOML".to_string();
    std::fs::remove_file(app_root.join("Cargo.toml")).expect("remove indexed manifest");
    let mut visited = BTreeSet::new();
    let mut dependencies = BTreeSet::new();
    collect_cargo_package_dependencies(&app_root, &index, &mut visited, &mut dependencies);
    assert!(
        dependencies.contains(&SourceDependencyV1::new(
            index.packages.get("support").expect("support package"),
            true,
        )),
        "cached dependency graph: {dependencies:#?}; index: {index:#?}"
    );
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
fn workspace_call_classification_derives_all_evidence_inputs_once() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "program": "rg",
            "args": ["needle", "src/foo.rs"],
            "workdir": "/repo"
        })
        .to_string(),
    };

    let classification =
        classify_workspace_tool_call("exec_command", &payload, Path::new("/fallback"));

    assert!(classification.observes_workspace);
    assert_eq!(classification.workspace_cwd, PathBuf::from("/repo"));
    assert_eq!(
        classification.source_dependencies,
        BTreeSet::from([SourceDependencyV1::new(
            Path::new("/repo/src/foo.rs"),
            false,
        )])
    );
}

#[test]
fn confirmed_performance_workspace_classification_runs_inline_without_runtime_handoff() {
    let temp = tempfile::tempdir().expect("tempdir");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({"package": "app", "workdir": temp.path()}).to_string(),
    };
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n",
    )
    .expect("workspace manifest");
    std::fs::create_dir_all(temp.path().join("app/src")).expect("app source");
    std::fs::write(
        temp.path().join("app/Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
    )
    .expect("app manifest");

    let classification = classify_workspace_tool_call("cargo_test", &payload, temp.path());
    assert!(
        classification
            .source_dependencies
            .contains(&SourceDependencyV1::new(&temp.path().join("app"), true,))
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
    assert!(
        response_item_has_valid_tool_history_receipt(&projection.items[0]),
        "projected item: {:#?}",
        projection.items[0]
    );
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
    assert!(serde_json::from_str::<ToolHistoryReceiptV2>(receipt).is_ok());
}

#[test]
fn token_efficiency_compact_v2_receipt_retains_integrity_and_accepts_legacy_v1() {
    let call_id = "call-1";
    let tracked = candidate(call_id, bounded_output());
    let rendered = tracked
        .derived
        .receipt
        .as_deref()
        .expect("eligible candidate receipt");
    let receipt: ToolHistoryReceiptV2 = serde_json::from_str(rendered).expect("compact v2 receipt");
    let value: serde_json::Value = serde_json::from_str(rendered).expect("receipt JSON");

    assert_eq!(receipt.version, RECEIPT_VERSION);
    assert_eq!(receipt.sha256, tracked.artifact_sha256);
    assert_eq!(receipt.sha256.len(), 64);
    assert!(value.get("artifact").is_none());
    assert!(value.get("original").is_none());
    assert!(value.get("retrieval").is_none());
    assert!(response_item_has_valid_tool_history_receipt(&text_output(
        call_id,
        rendered.to_string(),
    )));

    let legacy = ToolHistoryReceiptV1 {
        version: LEGACY_RECEIPT_VERSION,
        receipt_id: tracked.derived.receipt_id.clone(),
        call_id: call_id.to_string(),
        tool_identity: tracked.tool_identity.clone(),
        semantic_class: tracked.semantic_class.clone(),
        source_dependencies_current: true,
        digest: receipt.digest,
        artifact: ReceiptArtifact {
            artifact_id: tracked.artifact_id.clone(),
            byte_start: 0,
            byte_end: tracked.artifact_bytes,
            sha256: tracked.artifact_sha256.clone(),
            complete: true,
        },
        original: ReceiptOriginalSize {
            bytes: tracked.artifact_bytes,
            approximate_tokens: tracked.original_tokens,
        },
        retrieval: ReceiptRetrieval {
            tool: "read_tool_output".to_string(),
            instruction: "Recover the exact artifact.".to_string(),
        },
    };
    let legacy = serde_json::to_string(&legacy).expect("legacy receipt JSON");

    assert!(response_item_has_valid_tool_history_receipt(&text_output(
        call_id,
        legacy.clone(),
    )));
    assert!(approx_token_count(rendered) < approx_token_count(&legacy));
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
fn tool_history_admission_reserves_competing_results_before_spending_the_shared_budget() {
    let older = "older ".repeat(1_000);
    let newest = "x ".repeat(MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET);
    assert_eq!(
        approx_token_count(&newest),
        MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET
    );
    let mut state = ToolHistoryState::default();
    state.register(candidate("older-call", older.clone()));
    state.register(candidate("newest-call", newest.clone()));

    let projection = state.project(Arc::from([
        function_call("older-call"),
        text_output("older-call", older),
        function_call("newest-call"),
        text_output("newest-call", newest),
    ]));

    for call_id in ["older-call", "newest-call"] {
        assert!(projection.items.iter().any(|item| {
            matches!(item, ResponseItem::FunctionCall { call_id: projected, .. } if projected == call_id)
        }));
        assert!(
            projection
                .items
                .iter()
                .filter_map(textual_output_identity)
                .any(|(projected, _)| projected == call_id)
        );
    }
    assert!(
        projection
            .items
            .iter()
            .filter_map(textual_output_identity)
            .map(|(_, output)| approx_token_count(output))
            .sum::<usize>()
            <= MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET
    );
}

#[test]
fn tool_history_admission_preserves_newest_unconsumed_pair_over_budget() {
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

    assert!(projected_tokens > MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET);
    assert!(projection.items.len() < 160);
    let fallback_tokens = projection
        .unreplaced_items
        .iter()
        .filter_map(textual_output_identity)
        .map(|(_, output)| approx_token_count(output))
        .sum::<usize>();
    assert!(fallback_tokens > MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET);
    let (call_id, output) = projection
        .items
        .iter()
        .find_map(textual_output_identity)
        .expect("newest unconsumed output");
    assert_eq!(call_id, "call-79");
    assert!(output.starts_with("result-79 "));
    assert!(projection.items.iter().any(|item| {
        matches!(
            item,
            ResponseItem::FunctionCall { call_id, .. } if call_id == "call-79"
        )
    }));
}

#[test]
fn tool_history_admission_preserves_newest_unconsumed_non_text_content() {
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

    assert_eq!(projection.items.len(), 2);
    assert_eq!(projection.unreplaced_items.len(), 2);
    assert!(projection.substitutions.is_empty());
}

#[test]
fn tool_history_admission_keeps_small_consumed_raw_output_when_receipt_costs_more() {
    let call_id = "small-call";
    let output = "ok".to_string();
    let canonical: Arc<[ResponseItem]> = Arc::from([text_output(call_id, output.clone())]);
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, output.clone()));
    assert!(state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));

    let projection = state.project(canonical);

    assert!(projection.substitutions.is_empty());
    assert_eq!(
        textual_output_identity(&projection.items[0]),
        Some((call_id, output.as_str()))
    );
}

#[test]
fn tool_history_admission_keeps_in_budget_consumed_output_below_savings_thresholds() {
    let call_id = "threshold-call";
    let output = "x ".repeat(MINIMUM_RAW_TOKENS as usize);
    let raw_tokens = approx_token_count(&output);
    assert_eq!(raw_tokens, MINIMUM_RAW_TOKENS as usize);
    assert!(raw_tokens <= MODEL_VISIBLE_TOOL_RESULT_TOKEN_BUDGET);

    let canonical: Arc<[ResponseItem]> =
        Arc::from([function_call(call_id), text_output(call_id, output.clone())]);
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, output));
    assert!(state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));

    let stored = &state.candidates[call_id];
    let (_, _, admission_receipt_tokens) = stored
        .admission_receipt()
        .expect("complete candidate has an admission receipt");
    assert!(admission_receipt_tokens <= raw_tokens as u64);
    assert!(stored.receipt().is_none());

    let projection = state.project(Arc::clone(&canonical));

    assert_eq!(projection.items, canonical);
    assert!(projection.substitutions.is_empty());
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
                && serde_json::from_str::<ToolHistoryReceiptV2>(output).is_ok()
        })
    }));
}

#[test]
fn tool_history_admission_budgets_structured_tool_search_pairs() {
    let older = tool_search_pair("search-older", 48_000);
    let latest = tool_search_pair("search-latest", 48_000);
    let canonical: Arc<[ResponseItem]> = Arc::from([
        older[0].clone(),
        older[1].clone(),
        latest[0].clone(),
        latest[1].clone(),
    ]);

    let projection = ToolHistoryState::default().project(Arc::clone(&canonical));

    assert_eq!(projection.items.len(), 4);
    let receipts = projection
        .items
        .iter()
        .filter_map(tool_search_receipt)
        .collect::<Vec<_>>();
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().all(|receipt| {
        receipt.complete
            && receipt.result_count == 1
            && receipt.omitted_result_count == Some(0)
            && receipt.ordered_tool_identities.len() == 1
            && receipt.arguments["query"] == "example"
    }));
    assert!(projection.unreplaced_items.is_empty());
}

#[test]
fn tool_search_receipt_caps_all_argument_fields_and_binds_semantics() {
    let mut pair = tool_search_pair("search", 48_000);
    let ResponseItem::ToolSearchCall { arguments, .. } = &mut pair[0] else {
        panic!("expected search call");
    };
    *arguments = serde_json::json!({
        "query": "q".repeat(20_000),
        "namespace": "n".repeat(20_000),
        "limit": ["large".repeat(20_000)],
        "cursor": "c".repeat(20_000),
    });

    let projection = ToolHistoryState::default().project(Arc::from(pair));
    let receipt = projection
        .items
        .iter()
        .find_map(tool_search_receipt)
        .expect("bounded search receipt");
    let rendered = serde_json::to_string(&receipt).expect("serialize receipt");
    assert!(approx_token_count(&rendered) <= RECEIPT_MAX_TOKENS);
    assert!(receipt.arguments.get("query_sha256").is_some());
    assert!(receipt.arguments.get("namespace_sha256").is_some());
    assert!(receipt.arguments.get("limit_sha256").is_some());
    assert!(receipt.arguments.get("cursor_sha256").is_some());

    let mut changed = receipt.clone();
    changed.status = "failed".to_string();
    assert_ne!(
        receipt.receipt_id,
        tool_search_receipt_id(
            &changed.call_id,
            &changed.status,
            &changed.execution,
            &changed.arguments,
            &changed.result_set_sha256,
            changed.result_count,
            changed.omitted_result_count,
            changed.complete,
            changed.omitted_identity_count,
        )
    );
}

#[test]
fn structured_tool_search_negative_evidence_has_failure_priority() {
    let tool = serde_json::json!({"name": "matching-tool"});

    assert_eq!(
        tool_search_admission_priority("failed", std::slice::from_ref(&tool)),
        1
    );
    assert_eq!(tool_search_admission_priority("completed", &[]), 1);
    assert_eq!(tool_search_admission_priority("completed", &[tool]), 2);
}

#[test]
fn structured_tool_search_negative_evidence_precedes_success_under_budget() {
    let mut failed = tool_search_pair("search-failed", 28_000);
    let ResponseItem::ToolSearchCall { status, .. } = &mut failed[0] else {
        panic!("expected search call");
    };
    *status = Some("failed".to_string());
    let ResponseItem::ToolSearchOutput { status, .. } = &mut failed[1] else {
        panic!("expected search output");
    };
    *status = "failed".to_string();
    let successful = tool_search_pair("search-success", 28_000);

    let projection = ToolHistoryState::default().project(Arc::from([
        failed[0].clone(),
        failed[1].clone(),
        successful[0].clone(),
        successful[1].clone(),
    ]));

    let failed_output = projection
        .items
        .iter()
        .find(|item| item_call_id(item) == Some("search-failed"))
        .and_then(|_| {
            projection.items.iter().find(|item| {
                matches!(
                    item,
                    ResponseItem::ToolSearchOutput { call_id: Some(call_id), .. }
                        if call_id == "search-failed"
                )
            })
        })
        .expect("failed output retained");
    assert!(tool_search_receipt(failed_output).is_none());
    assert!(projection.items.iter().any(|item| {
        matches!(
            item,
            ResponseItem::ToolSearchOutput { call_id: Some(call_id), .. }
                if call_id == "search-success"
        ) && tool_search_receipt(item).is_some()
    }));
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
    first.consumed_by_generation = Some(ModelGenerationId {
        turn_id: "turn-1".to_string(),
        ordinal: 0,
    });
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
fn tool_history_admission_preserves_unconsumed_identical_parallel_pairs() {
    let bounded = bounded_output();
    let supersession_identity = format!(
        "functions.exec:{}:{}",
        sha256(b"same action"),
        sha256(b"same result")
    );
    let canonical: Arc<[ResponseItem]> = Arc::from([
        function_call("call-1"),
        text_output("call-1", bounded.clone()),
        function_call("call-2"),
        text_output("call-2", bounded.clone()),
        function_call("call-3"),
        text_output("call-3", bounded.clone()),
    ]);
    let mut state = ToolHistoryState::default();
    for call_id in ["call-1", "call-2", "call-3"] {
        let mut candidate = candidate(call_id, bounded.clone());
        candidate.supersession_identity = Some(supersession_identity.clone());
        state.register(candidate);
    }

    let projection = state.project(canonical);

    assert_eq!(projection.items.len(), 6);
    for call_id in ["call-1", "call-2", "call-3"] {
        assert_eq!(
            projection
                .items
                .iter()
                .filter(|item| item_call_id(item).is_some_and(|id| id == call_id))
                .count(),
            2,
            "call/output pair should remain visible for {call_id}"
        );
    }
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
            if serde_json::from_str::<ToolHistoryReceiptV2>(text).is_ok()
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
fn mcp_multi_text_content_is_canonicalized_and_receipted_without_losing_modalities() {
    let call_id = "mcp-multi-text";
    let first = "first section\n".repeat(400);
    let second = "second section\n".repeat(400);
    let bounded = format!("{first}\n{second}");
    let canonical: Arc<[ResponseItem]> = Arc::from([ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: FunctionCallOutputPayload::from_content_items(vec![
            FunctionCallOutputContentItem::InputText { text: first },
            FunctionCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,aW1hZ2U=".to_string(),
                detail: None,
            },
            FunctionCallOutputContentItem::InputText { text: second },
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
        panic!("expected content items");
    };
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
            .count(),
        1
    );
    assert!(items.iter().any(|item| matches!(
        item,
        FunctionCallOutputContentItem::InputText { text }
            if serde_json::from_str::<ToolHistoryReceiptV2>(text).is_ok()
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        FunctionCallOutputContentItem::InputImage { image_url, .. }
            if image_url == "data:image/png;base64,aW1hZ2U="
    )));
    assert!(items.iter().any(|item| matches!(
        item,
        FunctionCallOutputContentItem::EncryptedContent { encrypted_content }
            if encrypted_content == "opaque"
    )));
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
    let mut sha_tampered: serde_json::Value =
        serde_json::from_str(receipt).expect("valid receipt JSON");
    sha_tampered["sha256"] = serde_json::Value::String("b".repeat(64));
    assert!(!response_item_has_valid_tool_history_receipt(&text_output(
        call_id,
        serde_json::to_string(&sha_tampered).expect("serialize SHA-tampered receipt"),
    )));
}

#[test]
fn legacy_tool_history_ledger_keys_remain_compatible() {
    let bounded = bounded_output();
    let state = ToolHistoryState {
        candidates: BTreeMap::from([("call-1".to_string(), candidate("call-1", bounded))]),
        workspace_evidence: BTreeMap::new(),
        non_workspace_code_mode_calls: BTreeSet::new(),
        artifact_call_ids: BTreeMap::new(),
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

#[test]
fn compaction_artifact_reference_survives_history_replacement() {
    let call_id = "call-1";
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, bounded_output()));
    let compacted_summary = text_output(
        "compaction-summary",
        serde_json::to_string(&ToolHistoryArtifactPinV1 {
            version: 1,
            kind: "tool_history_artifact_pin".to_string(),
            artifact_id: "artifact-1".to_string(),
            bytes: 96_000,
            sha256: sha256(b"canonical artifact"),
        })
        .expect("serialize artifact pin"),
    );

    state.retain_for_history(&[compacted_summary]);

    assert!(state.candidates.contains_key(call_id));
}

#[test]
fn confirmed_performance_artifact_reference_walker_matches_borrowed_receipt_and_pin_objects() {
    let candidate = candidate("call-1", bounded_output());
    let (_, receipt, _) = candidate
        .admission_receipt()
        .expect("complete candidate has an admission receipt");
    let receipt: serde_json::Value = serde_json::from_str(receipt).expect("receipt JSON");
    let pin = serde_json::json!({
        "version": 1,
        "kind": "tool_history_artifact_pin",
        "artifact_id": candidate.artifact_id,
        "bytes": candidate.artifact_bytes,
        "sha256": candidate.artifact_sha256,
    });

    assert!(json_value_contains_artifact_reference(&receipt, &candidate));
    assert!(json_value_contains_artifact_reference(&pin, &candidate));
    assert!(!json_value_contains_artifact_reference(
        &serde_json::json!({ "artifact_id": "artifact-1" }),
        &candidate,
    ));
}

#[test]
fn plain_text_artifact_id_does_not_pin_tool_history() {
    let call_id = "call-1";
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, bounded_output()));

    state.retain_for_history(&[text_output(
        "compaction-summary",
        "The earlier output can be recovered from artifact-1.".to_string(),
    )]);

    assert!(!state.candidates.contains_key(call_id));
}

#[test]
fn compaction_tool_history_receipt_survives_history_replacement() {
    let call_id = "call-1";
    let candidate = candidate(call_id, bounded_output());
    let (_, receipt, _) = candidate
        .admission_receipt()
        .expect("complete candidate has an admission receipt");
    let receipt = receipt.to_string();
    let mut state = ToolHistoryState::default();
    state.register(candidate);

    state.retain_for_history(&[text_output("compaction-summary", receipt)]);

    assert!(state.candidates.contains_key(call_id));
}

#[tokio::test]
async fn tool_history_ledger_load_distinguishes_absence_corruption_and_version_mismatch() {
    let temp = tempfile::tempdir().expect("tempdir");
    assert!(matches!(
        load_tool_history_state_for_fork(temp.path(), "missing").await,
        ToolHistoryLoadOutcome::Missing
    ));

    let corrupt_path = ledger_path(temp.path(), "corrupt");
    std::fs::create_dir_all(corrupt_path.parent().expect("ledger parent"))
        .expect("create ledger parent");
    std::fs::write(&corrupt_path, b"{not-json").expect("write corrupt ledger");
    let corrupt = load_tool_history_state_for_fork(temp.path(), "corrupt").await;
    assert!(matches!(&corrupt, ToolHistoryLoadOutcome::Corrupt { .. }));
    let (_, warning) = corrupt.into_state_and_warning();
    assert!(warning.is_some_and(|warning| warning.contains("corrupt")));

    let unsupported_path = ledger_path(temp.path(), "unsupported");
    std::fs::write(
        unsupported_path,
        serde_json::to_vec(&ToolHistoryLedgerFile {
            version: LEDGER_VERSION.saturating_add(1),
            state: ToolHistoryState::default(),
        })
        .expect("serialize unsupported ledger"),
    )
    .expect("write unsupported ledger");
    assert!(matches!(
        load_tool_history_state_for_fork(temp.path(), "unsupported").await,
        ToolHistoryLoadOutcome::UnsupportedVersion {
            found,
            supported: LEDGER_VERSION,
            ..
        } if found == LEDGER_VERSION.saturating_add(1)
    ));
}

#[tokio::test]
async fn corrupt_own_thread_ledger_is_quarantined_but_fork_read_is_non_mutating() {
    let temp = tempfile::tempdir().expect("tempdir");
    let fork_path = ledger_path(temp.path(), "fork-source");
    std::fs::create_dir_all(fork_path.parent().expect("ledger parent"))
        .expect("create ledger parent");
    std::fs::write(&fork_path, b"{not-json").expect("write corrupt fork ledger");

    assert!(matches!(
        load_tool_history_state_for_fork(temp.path(), "fork-source").await,
        ToolHistoryLoadOutcome::Corrupt { .. }
    ));
    assert!(
        fork_path.exists(),
        "fork reads must not mutate the parent ledger"
    );

    let own_path = ledger_path(temp.path(), "own-thread");
    std::fs::write(&own_path, b"{not-json").expect("write corrupt own ledger");
    let outcome = load_tool_history_state(temp.path(), "own-thread").await;
    let ToolHistoryLoadOutcome::Corrupt { path, error } = outcome else {
        panic!("expected corrupt outcome");
    };
    assert!(!own_path.exists());
    assert!(path.exists());
    assert!(error.contains("quarantined"));
}

#[tokio::test]
async fn persist_empty_tool_history_state_skips_absent_checkpoint_and_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let thread_id = "empty-thread";

    persist_tool_history_state(temp.path(), thread_id, &ToolHistoryState::default())
        .await
        .expect("skip absent empty state");

    assert!(!ledger_path(temp.path(), thread_id).exists());
    assert!(!journal_path(temp.path(), thread_id).exists());
}

#[tokio::test]
async fn persist_empty_tool_history_state_clears_existing_checkpoint() {
    let temp = tempfile::tempdir().expect("tempdir");
    let thread_id = "thread";
    let mut stale_state = ToolHistoryState::default();
    stale_state.register_non_workspace_code_mode_call("stale-call".to_string());
    persist_tool_history_state(temp.path(), thread_id, &stale_state)
        .await
        .expect("persist stale ledger");
    let before = TOOL_HISTORY_DIRECTORY_SYNC_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed);

    persist_tool_history_state(temp.path(), thread_id, &ToolHistoryState::default())
        .await
        .expect("clear stale ledger");

    let after = TOOL_HISTORY_DIRECTORY_SYNC_ATTEMPTS.load(std::sync::atomic::Ordering::Relaxed);
    assert!(after > before);
    let restored =
        expect_loaded_tool_history(load_tool_history_state(temp.path(), thread_id).await);
    assert!(restored.non_workspace_code_mode_calls.is_empty());
}

#[tokio::test]
async fn persist_empty_tool_history_state_compacts_existing_journal() {
    let temp = tempfile::tempdir().expect("tempdir");
    let thread_id = "journal-thread";
    let mutations = [(
        1,
        ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
            call_id: "stale-call".to_string(),
        },
    )];
    persist_tool_history_mutations(temp.path(), thread_id, "writer", &mutations)
        .await
        .expect("persist stale journal");
    assert!(!ledger_path(temp.path(), thread_id).exists());
    assert!(journal_path(temp.path(), thread_id).exists());

    persist_tool_history_state(temp.path(), thread_id, &ToolHistoryState::default())
        .await
        .expect("compact stale journal");

    assert!(ledger_path(temp.path(), thread_id).exists());
    assert!(!journal_path(temp.path(), thread_id).exists());
    let restored =
        expect_loaded_tool_history(load_tool_history_state(temp.path(), thread_id).await);
    assert!(restored.non_workspace_code_mode_calls.is_empty());
}

#[tokio::test]
async fn mutation_journal_repairs_an_incomplete_tail_before_appending() {
    let temp = tempfile::tempdir().expect("tempdir");
    let thread_id = "journal-tail-thread";
    persist_tool_history_mutations(
        temp.path(),
        thread_id,
        "writer",
        &[(
            1,
            ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
                call_id: "first-call".to_string(),
            },
        )],
    )
    .await
    .expect("persist first mutation");
    std::fs::OpenOptions::new()
        .append(true)
        .open(journal_path(temp.path(), thread_id))
        .expect("open journal tail")
        .write_all(b"{incomplete")
        .expect("append incomplete journal tail");

    persist_tool_history_mutations(
        temp.path(),
        thread_id,
        "writer",
        &[(
            2,
            ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
                call_id: "second-call".to_string(),
            },
        )],
    )
    .await
    .expect("append after incomplete tail");

    let restored =
        expect_loaded_tool_history(load_tool_history_state_for_fork(temp.path(), thread_id).await);
    assert!(
        restored
            .non_workspace_code_mode_calls
            .contains("first-call")
    );
    assert!(
        restored
            .non_workspace_code_mode_calls
            .contains("second-call")
    );
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

    let loaded = expect_loaded_tool_history(
        load_tool_history_state_for_fork(temp.path(), source_thread_id).await,
    );
    assert_eq!(
        loaded
            .artifact_call_ids
            .get(&source_artifact_id)
            .map(String::as_str),
        Some(call_id)
    );
    let (forked, dropped) =
        remint_tool_history_state_for_fork(temp.path(), source_thread_id, target_thread_id, loaded)
            .await;
    assert_eq!(dropped, 0);
    let target_artifact_id = forked.candidates[call_id].artifact_id.clone();
    assert_eq!(target_artifact_id, source_artifact_id);
    assert_eq!(
        forked
            .artifact_call_ids
            .get(&target_artifact_id)
            .map(String::as_str),
        Some(call_id)
    );

    let forked = reconcile_tool_history_state(temp.path(), target_thread_id, forked).await;
    persist_tool_history_state(temp.path(), target_thread_id, &forked)
        .await
        .expect("persist target ledger");
    let restored =
        expect_loaded_tool_history(load_tool_history_state(temp.path(), target_thread_id).await);
    let projection = restored.project(canonical_history);
    assert_eq!(projection.substitutions.len(), 1);
    let ResponseItem::FunctionCallOutput { output, .. } = &projection.items[0] else {
        panic!("expected projected function output");
    };
    let FunctionCallOutputBody::Text(receipt) = &output.body else {
        panic!("expected receipt text");
    };
    let receipt: ToolHistoryReceiptV2 = serde_json::from_str(receipt).expect("receipt JSON");
    assert_eq!(receipt.artifact_id, target_artifact_id);
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

    let mut restored =
        expect_loaded_tool_history(load_tool_history_state(temp.path(), thread_id).await);
    assert!(restored.candidates.contains_key(call_id));
    restored.retain_for_history(&[]);
    let reconciled = reconcile_tool_history_state(temp.path(), thread_id, restored).await;
    persist_tool_history_state(temp.path(), thread_id, &reconciled)
        .await
        .expect("persist pruned ledger");
    assert!(
        expect_loaded_tool_history(load_tool_history_state(temp.path(), thread_id).await)
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

#[test]
fn receipt_and_candidate_fingerprints_are_cached_and_reused() {
    let candidate = candidate("cached-call", bounded_output());
    assert_eq!(
        candidate.derived.bounded_model_output_sha256,
        sha256(candidate.bounded_model_output.as_bytes())
    );
    assert_eq!(
        candidate.derived.bounded_model_output_tokens,
        u64::try_from(approx_token_count(&candidate.bounded_model_output)).unwrap_or(u64::MAX)
    );

    let first = candidate
        .admission_receipt()
        .expect("complete candidate has an admission receipt");
    let second = candidate
        .admission_receipt()
        .expect("cached admission receipt remains available");
    assert_eq!(first, second);
    assert_eq!(first.0, candidate.derived.receipt_id);
    assert!(std::ptr::eq(first.1.as_ptr(), second.1.as_ptr()));
    assert_eq!(
        first.2,
        u64::try_from(approx_token_count(first.1)).unwrap_or(u64::MAX)
    );
}

#[test]
fn affected_path_index_preserves_source_dependency_overlap_semantics() {
    let dependencies = [
        SourceDependencyV1 {
            path: "src/exact.rs".to_string(),
            recursive: false,
        },
        SourceDependencyV1 {
            path: "src/tree".to_string(),
            recursive: true,
        },
        SourceDependencyV1 {
            path: "src/tree/leaf.rs".to_string(),
            recursive: false,
        },
    ];
    let affected_paths = BTreeSet::from([
        "docs/unrelated.md".to_string(),
        "src/exact.rs".to_string(),
        "src/tree/child.rs".to_string(),
    ]);

    for dependency in dependencies {
        let linear_result = affected_paths
            .iter()
            .any(|path| source_dependency_overlaps(&dependency, path));
        assert_eq!(
            affected_paths_overlap_dependency(&affected_paths, &dependency),
            linear_result,
            "indexed lookup changed overlap semantics for {dependency:?}"
        );
    }
    assert!(affected_paths_overlap_dependency(
        &BTreeSet::from(["src".to_string()]),
        &SourceDependencyV1 {
            path: "src/nested/file.rs".to_string(),
            recursive: false,
        }
    ));
}

#[test]
fn textual_output_identity_borrows_single_text_outputs() {
    let output = text_output("borrowed-call", "model-visible text".to_string());
    let (call_id, text) =
        canonical_textual_output_identity(&output).expect("text output has an identity");
    assert_eq!(call_id, "borrowed-call");
    assert!(matches!(text, Cow::Borrowed("model-visible text")));
}

#[test]
fn artifact_index_is_deterministic_and_rebuilt_after_retention() {
    assert_eq!(read_tool_output_artifact_id("not-json"), None);
    assert_eq!(read_tool_output_artifact_id(r#"{"other":"value"}"#), None);
    assert_eq!(
        read_tool_output_artifact_id(r#"{"artifact_id":"artifact-1"}"#).as_deref(),
        Some("artifact-1")
    );
    let mut state = ToolHistoryState::default();
    state.register(candidate("call-2", bounded_output()));
    state.register(candidate("call-1", bounded_output()));
    assert_eq!(
        state
            .artifact_call_ids
            .get("artifact-1")
            .map(String::as_str),
        Some("call-1")
    );

    let mut retrieval = named_function_call("retrieval", "read_tool_output");
    let ResponseItem::FunctionCall { arguments, .. } = &mut retrieval else {
        panic!("helper must return a function call");
    };
    *arguments = serde_json::json!({"artifact_id": "artifact-1"}).to_string();
    state.retain_for_history(&[retrieval]);

    assert!(state.candidates.contains_key("call-1"));
    assert!(!state.candidates.contains_key("call-2"));
    assert_eq!(
        state
            .artifact_call_ids
            .get("artifact-1")
            .map(String::as_str),
        Some("call-1")
    );
}

#[test]
fn replacing_selected_candidate_repairs_only_affected_artifact_mappings() {
    let mut state = ToolHistoryState::default();
    state.register(candidate("call-2", bounded_output()));
    state.register(candidate("call-1", bounded_output()));

    let mut replacement = candidate("call-1", bounded_output());
    replacement.artifact_id = "artifact-2".to_string();
    state.register(replacement);

    assert_eq!(
        state
            .artifact_call_ids
            .get("artifact-1")
            .map(String::as_str),
        Some("call-2")
    );
    assert_eq!(
        state
            .artifact_call_ids
            .get("artifact-2")
            .map(String::as_str),
        Some("call-1")
    );
}

#[test]
fn workspace_observation_from_argument_parts_matches_payload_classifier() {
    for arguments in [
        r#"{"cmd":"rg -n needle src"}"#,
        r#"{"cmd":"git status --short"}"#,
        r#"{"cmd":"cargo fmt"}"#,
        "not-json",
    ] {
        let payload = ToolPayload::Function {
            arguments: arguments.to_string(),
        };
        assert_eq!(
            tool_call_observes_workspace_parts("exec_command", arguments),
            tool_call_observes_workspace("exec_command", &payload),
            "argument-only classification diverged for {arguments}"
        );
    }
}

#[test]
fn borrowed_ledger_serialization_matches_owned_compatibility_shape() {
    let mut state = ToolHistoryState::default();
    state.register(candidate("serialized-call", bounded_output()));
    let owned = serde_json::to_vec(&ToolHistoryLedgerFile {
        version: LEDGER_VERSION,
        state: state.clone(),
    })
    .expect("serialize owned compatibility envelope");
    let borrowed = serde_json::to_vec(&ToolHistoryLedgerRef {
        version: LEDGER_VERSION,
        state: &state,
    })
    .expect("serialize borrowed ledger envelope");

    assert_eq!(borrowed, owned);
}
