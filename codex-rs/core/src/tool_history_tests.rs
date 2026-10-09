use super::*;

#[test]
fn salience_receipt_spends_space_on_all_small_diagnostic_groups() {
    for decisive in 0..9 {
        let output = (0..9).map(|index| {
            let label = if index == decisive { "decisive" } else { "other" };
            format!("error: {label}{index}\n\n\n\n")
        }).collect::<String>();
        let record = candidate("small-groups", serde_json::json!({"output":output}).to_string());
        let digest = record.receipt_digest_input();
        assert_eq!(digest.matches("error:").count(), 9, "{digest}");
        let pin = record.artifact_pin_value().unwrap();
        let digest = pin["digest"].as_str().unwrap();
        assert!(digest.contains(&format!("decisive{decisive}")), "{digest}");
        assert!(approx_token_count(digest) <= RECEIPT_DIGEST_TARGET_TOKENS);
    }
}

#[test]
fn salience_evidence_budget_uses_sent_receipt_not_retained_payload() {
    let mut state = ToolHistoryState::default();
    state.register(candidate("large-source", "x".repeat(320_000)));
    let generation = ModelGenerationId { turn_id: "turn".into(), ordinal: 1 };
    // Only this compact representation was actually sent.
    state.mark_consumed_with_delta(&[text_output("large-source", "x".repeat(8_000))], generation.clone());
    assert_eq!(state.last_visible_tool_output_tokens, Some(2_000));
    assert_eq!(state.task_sensitive_tool_result_budget(100_000, 90_000, 8192), 3808);
    // A later request containing no tool output must not inherit the old cost.
    state.mark_consumed_with_delta(&[], generation);
    assert_eq!(state.last_visible_tool_output_tokens, Some(0));
    assert_eq!(state.task_sensitive_tool_result_budget(100_000, 90_000, 8192), 1808);
    let restored: ToolHistoryState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(restored.last_visible_tool_output_tokens, None);
    assert_eq!(restored.task_sensitive_tool_result_budget(100_000, 90_000, 8192), 1808);
}

#[test]
fn epistemic_unknown_dependency_scope_never_proves_currentness() {
    let revision = workspace_identity("unchanged");
    let output = text_output("unknown", "retained exact historical bytes".into());
    let observation = WorkspaceEvidenceObservation::from_response_item(
        Some(revision.clone()), &output, BTreeSet::new()).unwrap();
    assert!(!observation.is_current(Some(&revision), None));
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(observation);
    let projected = state.project_with_workspace_identity(
        Arc::from([function_call("unknown"), output]), Some(&revision));
    let (_, text) = textual_output_identity(&projected.items[1]).unwrap();
    let notice: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(notice["valid_for_current_workspace"], false);
    assert_eq!(notice["workspace_evidence_freshness"], "unknown");
}

#[tokio::test]
async fn epistemic_remote_receipts_require_local_claim_authentication() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let record = candidate("auth", bounded_output());
    let receipt = record.derived.receipt.clone().unwrap();
    let mut state = ToolHistoryState::default();
    state.register(record.clone());
    session.register_tool_history_candidate(record).await;
    let original = text_output("auth", receipt.clone());
    assert!(state.authenticates_receipt(&original));
    let value: serde_json::Value = serde_json::from_str(&receipt).unwrap();
    assert!(state.authenticates_receipt(&text_output("auth", serde_json::to_string_pretty(&value).unwrap())));
    for (key, value) in [
        ("successful", serde_json::json!(false)),
        ("source_dependencies_current", serde_json::json!(false)),
        ("digest", serde_json::json!("all tests verified")),
        ("evidence", serde_json::json!({"file_complete":true})),
        ("artifact_id", serde_json::json!("invented-artifact")),
        ("verification_claim", serde_json::json!("all integration tests passed")),
    ] {
        let mut altered: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        altered[key] = value;
        let altered = text_output("auth", altered.to_string());
        assert!(!state.authenticates_receipt(&altered), "{key}");
        let (retained, _, _) = crate::compact_remote::process_compacted_history_with_retained_input(
            &session, &turn, vec![function_call("auth"), altered], Vec::new(),
            &crate::compact::InitialContextInjection::DoNotInject).await.unwrap();
        assert!(!retained.iter().any(|item| textual_output_identity(item).is_some()), "{key}");
    }
    let (retained, _, _) = crate::compact_remote::process_compacted_history_with_retained_input(
        &session, &turn, vec![function_call("auth"), original.clone()], Vec::new(),
        &crate::compact::InitialContextInjection::DoNotInject).await.unwrap();
    assert!(retained.contains(&original));
}

#[test]
fn epistemic_search_receipt_binds_identity_order_and_count() {
    let pair = tool_search_pair("identity", 0);
    let (item, _) = tool_search_receipt_item(&pair[1], None).unwrap();
    let original = tool_search_receipt(&item).unwrap();
    assert!(original.is_valid("identity", "completed", "client"));
    for identities in [vec!["invented.admin".to_string()], Vec::new(),
        vec!["tool-identity".to_string(), "extra".to_string()]] {
        let mut changed = original.clone();
        changed.ordered_tool_identities = identities;
        assert!(!changed.is_valid("identity", "completed", "client"));
    }
    let mut legacy = original;
    legacy.version = 1;
    assert!(!legacy.is_valid("identity", "completed", "client"), "legacy identities were not bound");
}

#[test]
fn evidence_salience_read_pins_keep_source_scope_and_excerpt_after_restore() {
    let mut pins = Vec::new();
    for (call, environment, body) in [("one", "local", "AUTH_REQUIRED=1"),
        ("two", "local", "AUTH_REQUIRED=0"), ("three", "remote", "AUTH_REQUIRED=1")]
    {
        let text = format!("{body}\n{}", "source context\n".repeat(256));
        let hash = sha256(text.as_bytes());
        let output = serde_json::json!({"path":"/repo/config.rs", "environment_id":environment,
            "source_sha256":hash, "file_complete":true, "complete":true,
            "selection_status":"complete", "results":[{"status":"ok", "complete":true,
                "selector":{"kind":"lines", "start":1, "end":257}, "text":text}]}).to_string();
        let mut record = candidate(call, output.clone());
        record.tool_identity = "read_file".into();
        record.artifact_id = format!("artifact-{call}");
        let mut state = ToolHistoryState::default();
        state.register(record);
        let mut restored: ToolHistoryState = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
        restored.refresh_derived_and_indexes();
        let payload = restored.artifact_pin_payload_for_items(&[text_output(call, output)]).unwrap();
        assert!(approx_token_count(&payload) <= COMPACTION_ARTIFACT_PIN_TOKEN_BUDGET);
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let pin = payload["artifacts"][0].clone();
        assert_eq!(pin["evidence"]["environment_id"], environment);
        assert_eq!(pin["evidence"]["source_sha256"], hash);
        assert_eq!(pin["evidence"]["file_complete"], true);
        let digest = pin["digest"].as_str().unwrap();
        assert!(digest.starts_with("Partial source excerpt:"));
        assert!(digest.contains(body));
        assert!(approx_token_count(digest) <= RECEIPT_DIGEST_TARGET_TOKENS);
        pins.push(pin);
    }
    assert_ne!(pins[0]["digest"], pins[1]["digest"]);
    assert_ne!(pins[0]["evidence"], pins[2]["evidence"]);
    let output = serde_json::json!({"complete":false, "results":[
        {"status":"ok", "text":"successful sibling"},
        {"status":"not_found", "message":"missing selection"}]}).to_string();
    let mut record = candidate("partial", output);
    record.tool_identity = "read_file".into();
    assert!(record.receipt_digest_input().contains("missing selection"));
    let mut search = candidate("search", serde_json::json!({"results":[{"status":"ok",
        "value":{"hydrated_ranges":[{"text":"exact recovered fact"}]}}]}).to_string());
    search.tool_identity = "read_tool_output".into();
    assert!(search.receipt_digest_input().contains("exact recovered fact"));
    let unicode = "🦀".repeat(200);
    search.bounded_model_output = serde_json::json!({"results":[{"status":"ok", "text":unicode}]}).to_string();
    let digest = search.receipt_digest_input();
    let excerpt = digest.strip_prefix("Partial source excerpt:\n").unwrap();
    assert!(unicode.starts_with(excerpt));
    assert!(excerpt.len() < unicode.len());
    assert!(approx_token_count(search.artifact_pin_value().unwrap()["digest"].as_str().unwrap()) <= RECEIPT_DIGEST_TARGET_TOKENS);
}

#[test]
fn evidence_salience_receipt_keeps_terminal_cause_after_secondary_errors() {
    for groups in [9, 12, 100] {
        let mut output = String::new();
        for index in 0..groups - 1 {
            output.push_str(&format!("error: secondary {index}\ncontext\ncontext\ncontext\n"));
        }
        output.push_str("fatal: ROOT_CAUSE_SENTINEL\nactual: 17\nexpected: 9\n");
        let mut record = candidate("failure", serde_json::json!({"output":output, "exit_code":1}).to_string());
        record.successful = false;
        record.refresh_derived();
        let pin = record.artifact_pin_value().unwrap();
        let digest = pin["digest"].as_str().unwrap();
        assert!(digest.matches("error: secondary").count() <= 7);
        assert!(digest.contains("ROOT_CAUSE_SENTINEL"), "{digest}");
        assert!(digest.contains("actual: 17"), "{digest}");
        assert!(approx_token_count(digest) <= RECEIPT_DIGEST_TARGET_TOKENS);
    }
}

#[tokio::test]
async fn verified10_remote_compaction_storage_failure_does_not_install_history() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let mut items = Vec::new();
    for index in 0..COMPACTION_ARTIFACT_PIN_MAX_ITEMS + 5 {
        let call = format!("retained-{index}");
        let output = format!("retained canonical evidence {index}");
        let mut record = candidate(&call, output.clone());
        record.artifact_id = format!("artifact-{index}");
        session.register_tool_history_candidate(record).await;
        items.push(text_output(&call, output));
    }
    session.record_conversation_items(&turn, &items).await.unwrap();
    let before = session.clone_history().await;
    let window = session.current_window_id().await;
    let reference = session.reference_context_item().await;
    let home = session.codex_home().await;
    let blocked = home.join("tool-output").join(session.thread_id().to_string());
    assert!(!blocked.exists(), "the fixture must own a new artifact directory");
    std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();
    std::fs::write(&blocked, "not a directory").unwrap();
    let result = crate::compact_remote::process_compacted_history_with_retained_input(
        &session, &turn, vec![ResponseItem::Compaction {
            id: None, encrypted_content: "uninstalled".into(),
            internal_chat_message_metadata_passthrough: None,
        }], Vec::new(), &crate::compact::InitialContextInjection::DoNotInject,
    ).await;
    assert!(result.is_err());
    assert_eq!(session.current_window_id().await, window);
    assert_eq!(session.reference_context_item().await, reference);
    assert_eq!(session.clone_history().await.raw_items(), before.raw_items());
}

#[test]
fn verified10_compaction_preserves_recency_across_sidecars() {
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for index in 0..40 {
        let id = format!("opaque-{index:03}");
        let mut record = candidate(&id, bounded_output());
        record.artifact_id = format!("artifact-{index}");
        state.register(record);
        items.push(text_output(&id, bounded_output()));
    }
    let first = state.artifact_pin_payload_for_items(&items).unwrap();
    let first_value: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(first_value["artifacts"][0]["call_id"], "opaque-039");
    let second = state.artifact_pin_payload_for_items(&[text_output("sidecar", first)]).unwrap();
    let second_value: serde_json::Value = serde_json::from_str(&second).unwrap();
    let call_ids = |value: &serde_json::Value| value["artifacts"].as_array().unwrap()
        .iter().map(|pin| pin["call_id"].clone()).collect::<Vec<_>>();
    assert_eq!(call_ids(&second_value), call_ids(&first_value));
    let mut record = candidate("aaa-newest", bounded_output());
    record.artifact_id = "latest-artifact".into();
    state.register(record);
    let third = state.artifact_pin_payload_for_items(&[
        text_output("sidecar", second), text_output("aaa-newest", bounded_output()),
    ]).unwrap();
    let third: serde_json::Value = serde_json::from_str(&third).unwrap();
    assert_eq!(third["artifacts"][0]["call_id"], "aaa-newest");
}

#[test]
fn verified10_read_status_recovers_all_snapshots_and_disjoint_ranges() {
    let path = PathBuf::from("never-open-this-source");
    let mut state = ToolHistoryState::default();
    for version in 0..9 {
        let output = serde_json::json!({
            "path":path, "environment_id":"local", "canonical_uri":"file:///source",
            "source_sha256":format!("hash-{version}"), "canonical_bytes":140,
            "results":(0..70).map(|index| serde_json::json!({
                "status":"ok", "complete":true, "text":"x",
                "canonical_range":{"start":index*2,"end":index*2+1}
            })).collect::<Vec<_>>()
        });
        let mut entry = candidate(&format!("read-{version}"), output.to_string());
        entry.tool_identity = "read_file".into();
        state.register(entry);
    }
    let report = state.read_status(&[path.clone()], Some("local"), &[]);
    assert_eq!(report["paths"][0]["next_snapshot_offset"], 8);
    let query = ReadStatusQuery { snapshot_offset:8, ..Default::default() };
    let tail = state.read_status_page(&[path.clone()], Some("local"), &[], &query);
    assert_eq!(tail["paths"][0]["snapshots"].as_array().unwrap().len(), 1);
    let snapshot = &tail["paths"][0]["snapshots"][0];
    assert_eq!(report["paths"][0]["snapshots"][0]["source_sha256"], "hash-8");
    assert_eq!(snapshot["source_sha256"], "hash-0");
    assert_eq!(snapshot["next_range_offset"], 64);
    let query = ReadStatusQuery { snapshot_id: snapshot["snapshot_id"].as_str().map(str::to_string),
        range_offset:64, ..Default::default() };
    let tail_ranges = state.read_status_page(&[path.clone()], Some("local"), &[], &query);
    let tail_snapshot = &tail_ranges["paths"][0]["snapshots"][0];
    assert_eq!(tail_snapshot["obtained_ranges"].as_array().unwrap().len(), 6);
    assert_eq!(tail_snapshot["unread_ranges"].as_array().unwrap().len(), 6);
    assert!(tail_snapshot["next_range_offset"].is_null());
    for version in 0..9 {
        let query = ReadStatusQuery { source_sha256:Some(format!("hash-{version}")), ..Default::default() };
        let selected = state.read_status_page(&[path.clone()], Some("local"), &[], &query);
        assert_eq!(selected["paths"][0]["snapshots"].as_array().unwrap().len(), 1);
    }
}

#[test]
fn read_status_prioritizes_observation_recency_and_selects_historical_hashes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.rs");
    let mut state = ToolHistoryState::default();
    for index in 0..12 {
        let call = format!("call-{}", 100 - index);
        let output = serde_json::json!({"path":path, "environment_id":"local", "canonical_uri":"file:///source.rs",
            "source_sha256":format!("hash-{index:02}"), "canonical_bytes":1,
            "results":[{"status":"ok", "complete":true, "canonical_range":{"start":0,"end":1}, "text":"x"}]});
        let mut entry = candidate(&call, output.to_string());
        entry.tool_identity = "read_file".into();
        entry.source_dependencies_current = false;
        ToolHistoryMutation::RegisterCandidate { candidate: entry }.apply(&mut state);
    }
    let persisted = serde_json::to_value(&state).unwrap();
    let mut state: ToolHistoryState = serde_json::from_value(persisted.clone()).unwrap();
    let latest = state.read_status(&[path.clone()], Some("local"), &[]);
    assert_eq!(latest["paths"][0]["snapshots"][0]["source_sha256"], "hash-11");
    assert_eq!(latest["paths"][0]["snapshots"][0]["freshness"], "invalidated");
    assert_eq!(latest["paths"][0]["omitted_snapshots"], 4);
    let selected = state.read_status_page(&[path.clone()], Some("local"), &[], &ReadStatusQuery {
        source_sha256: Some("hash-00".into()), ..Default::default()
    });
    assert_eq!(selected["paths"][0]["snapshots"].as_array().unwrap().len(), 1);
    assert_eq!(selected["paths"][0]["snapshots"][0]["source_sha256"], "hash-00");
    assert_eq!(serde_json::to_value(&state).unwrap(), persisted);
    let mut legacy = persisted;
    legacy.as_object_mut().unwrap().remove("observation_order");
    let legacy: ToolHistoryState = serde_json::from_value(legacy).unwrap();
    let report = legacy.read_status(&[path], Some("local"), &[]);
    assert_eq!(report["paths"][0]["snapshots"][0]["observation_recency"], "unknown");
    state.retain_for_history(&[]);
    assert!(state.observation_order.is_empty());
}

#[tokio::test]
async fn dependency_notices_distinguish_changed_current_and_unverified_paths() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a.rs");
    let b = root.path().join("b.rs");
    std::fs::write(&a, "a").unwrap();
    std::fs::write(&b, "b").unwrap();
    let cache = GitWorkspaceCache::new();
    let watches = cache.begin_source_path_change_observations(root.path(), &[(a.clone(), false), (b.clone(), false)])
        .await.unwrap();
    let mut identity = workspace_identity("captured");
    identity.repository_root = Some(root.path().to_string_lossy().into_owned());
    let output = serde_json::json!({"environment_id":"local"}).to_string();
    let observation = WorkspaceEvidenceObservation::from_response_item(Some(identity.clone()),
        &text_output("read", output.clone()), BTreeSet::from([
            SourceDependencyV1::new(&a, false), SourceDependencyV1::new(&b, false),
        ])).unwrap().with_source_path_observations(watches);
    cache.note_host_workspace_mutation_paths(root.path(), &["a.rs".into()]).await;
    let scope = observation.dependency_notice(&output, Some(&identity), Some(&cache));
    assert_eq!(scope["dependencies"][0]["path"], SourceDependencyV1::new(&a, false).path);
    assert_eq!(scope["dependencies"][0]["freshness"], "changed");
    assert_eq!(scope["dependencies"][1]["freshness"], "current");
    assert_eq!(scope["dependencies"][0]["environment_id"], "local");
    let unknown = observation.dependency_notice(&output, Some(&identity), None);
    assert!(unknown["dependencies"].as_array().unwrap().iter().all(|row| row["freshness"] == "unknown"));
    let mut bounded = observation;
    bounded.source_dependencies = (0..12).map(|index|
        SourceDependencyV1::new(&root.path().join(format!("{index}.rs")), false)).collect();
    assert_eq!(bounded.dependency_notice("{}", None, None)["omitted_dependencies"], 4);
}

#[tokio::test]
async fn verified10_segmented_history_survives_resume_fork_and_boundary_recovery() {
    let home = tempfile::tempdir().unwrap();
    let boundary = 16 * 1024 * 1024;
    let canonical = CanonicalToolResult::text("x".repeat(boundary + 32));
    let artifact = create_canonical_output_artifact(home.path(), "parent", &canonical).await;
    assert!(artifact.complete);
    let id = artifact.artifact_id().unwrap();
    protect_active_tool_history_artifact(home.path(), "parent", &id, canonical.exact_bytes, &canonical.sha256).await.unwrap();
    let mut record = candidate("segmented", bounded_output());
    record.artifact_id = id.clone();
    record.artifact_bytes = canonical.exact_bytes;
    record.artifact_sha256 = canonical.sha256;
    record.consumed_by_generation = Some(ModelGenerationId { turn_id: "turn".into(), ordinal: 1 });
    let mut state = ToolHistoryState::default();
    state.register(record);
    persist_tool_history_state(home.path(), "parent", &state).await.unwrap();
    let resumed = expect_loaded_tool_history(load_tool_history_state(home.path(), "parent").await);
    assert_eq!(resumed.candidates.len(), 1);
    let (forked, dropped) = remint_tool_history_state_for_fork(home.path(), "parent", "child", resumed).await;
    assert_eq!(dropped, 0);
    persist_tool_history_state(home.path(), "child", &forked).await.unwrap();
    let restored = expect_loaded_tool_history(load_tool_history_state(home.path(), "child").await);
    assert_eq!(restored.candidates.len(), 1);
    let result = crate::tools::command_output_artifact::read_tool_output_selectors(
        home.path(), "child", &id, vec![crate::tools::command_output_artifact::ToolOutputSelector::Bytes {
            start: boundary as u64 - 16, end: boundary as u64 + 16,
        }],
    ).await.unwrap();
    assert!(result.complete);
    assert_eq!(result.results[0].text.as_deref(), Some("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"));
    let segment = home.path().join("tool-output/child").join(format!("{id}.segment-000001.log"));
    // Use the actual advertised segment filename, without assuming its format.
    let segment = std::fs::read_dir(segment.parent().unwrap()).unwrap().map(|entry| entry.unwrap().path())
        .find(|path| path.file_name().unwrap().to_string_lossy().starts_with(&format!("{id}.segment-"))).unwrap();
    std::fs::write(segment, "corrupt").unwrap();
    assert!(crate::tools::command_output_artifact::verify_tool_history_artifact(
        home.path(), "child", &id, canonical.exact_bytes, &restored.candidates["segmented"].artifact_sha256,
    ).await.is_err());
}

#[test]
fn verified10_nested_read_coverage_survives_compaction_without_payload() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.rs");
    let output = serde_json::json!({"path":path, "environment_id":"local", "canonical_uri":"file:///source.rs",
        "source_sha256":"hash", "canonical_bytes":8_000, "artifact_id":"source-snapshot",
        "retained_artifact_complete":true, "file_complete":true,
        "results":[{"status":"ok", "complete":true, "canonical_range":{"start":0,"end":8_000},
            "text":"x".repeat(8_000)}]});
    let pin = compact_read_evidence(&output);
    assert!(pin.to_string().len() < 1_024);
    let mut state = ToolHistoryState::default();
    let observation = WorkspaceEvidenceObservation::from_response_item(None,
        &text_output("nested", output.to_string()), BTreeSet::from([SourceDependencyV1::new(&path, false)])).unwrap();
    ToolHistoryMutation::RegisterWorkspaceEvidence { observation }.apply(&mut state);
    ToolHistoryMutation::RegisterArtifactOrigin { artifact_id:"source-snapshot".into(), call_id:"nested".into(),
        bytes:8_000, sha256:"hash".into() }.apply(&mut state);
    ToolHistoryMutation::RegisterCodeModeNestedEvidence { parent_call_id:"cell".into(), call_id:"nested".into(), output:pin.to_string() }.apply(&mut state);
    let before = state.read_status(&[path.clone()], Some("local"), &[]);
    assert_eq!(before["paths"][0]["snapshots"][0]["coverage"], "full");
    state.retain_for_history(&[text_output("compact", pin.to_string())]);
    let restored: ToolHistoryState = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
    assert_eq!(restored.read_status(&[path], Some("local"), &[]), before);
}

#[tokio::test]
async fn adjacent_recovery_pages_coalesce_before_persistence_bound() {
    let temp = tempfile::tempdir().unwrap();
    let canonical = CanonicalToolResult::text("x".repeat(650));
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let mut record = candidate("source", bounded_output());
    record.artifact_id = artifact.artifact_id().unwrap();
    record.artifact_bytes = canonical.exact_bytes;
    record.artifact_sha256 = canonical.sha256;
    let id = record.artifact_id.clone();
    let mut state = ToolHistoryState::default();
    state.register(record);
    for page in 0..65 {
        assert!(ToolHistoryMutation::RecordArtifactRecovery { artifact_id:id.clone(),
            recovery_call_id:format!("page-{page}"), selectors:vec![serde_json::json!({
                "kind":"bytes", "start":page * 10, "end":(page + 1) * 10})] }.apply(&mut state));
    }
    let expected = vec![serde_json::json!({"kind":"bytes", "start":0, "end":650})];
    assert_eq!(state.recovered_ranges["source"], expected);
    persist_tool_history_state(temp.path(), "thread", &state).await.unwrap();
    let mut restored = expect_loaded_tool_history(load_tool_history_state(temp.path(), "thread").await);
    assert_eq!(restored.recovered_ranges["source"], expected);
    assert!(!ToolHistoryMutation::RecordArtifactRecovery { artifact_id:id,
        recovery_call_id:"source".into(), selectors:vec![serde_json::json!({
            "kind":"bytes", "start":10, "end":20})] }.apply(&mut restored));
}

#[test]
fn forgotten_recovery_coverage_is_unknown_not_unread() {
    let path = PathBuf::from("source.txt");
    let mut record = candidate("source", serde_json::json!({"path":path, "source_sha256":"hash",
        "canonical_bytes":200, "results":[]}).to_string());
    record.tool_identity = "read_file".into();
    let id = record.artifact_id.clone();
    let mut state = ToolHistoryState::default();
    state.register(record);
    for page in 0..65 {
        ToolHistoryMutation::RecordArtifactRecovery { artifact_id:id.clone(),
            recovery_call_id:"page".into(), selectors:vec![serde_json::json!({
                "kind":"bytes", "start":page * 2, "end":page * 2 + 1})] }.apply(&mut state);
    }
    assert_eq!(state.recovered_ranges["source"].len(), 64);
    let restored: ToolHistoryState = serde_json::from_value(serde_json::to_value(state).unwrap()).unwrap();
    let report = restored.read_status(&[path], None, &[]);
    let snapshot = &report["paths"][0]["snapshots"][0];
    assert_eq!(snapshot["coverage_history_complete"], false);
    assert_eq!(snapshot["unread_ranges"], serde_json::json!([]));
    assert!(!snapshot["unknown_ranges"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn stale_validation_pin_survives_compaction_and_restart_without_rerun() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("input.rs");
    let text = bounded_output();
    let canonical = CanonicalToolResult::text(text.clone());
    let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
    let mut record = candidate("validation", text.clone());
    record.artifact_id = artifact.artifact_id().unwrap();
    record.artifact_bytes = canonical.exact_bytes;
    record.artifact_sha256 = canonical.sha256;
    record.source_dependencies.insert(SourceDependencyV1::new(&source, false));
    record.consumed_by_generation = Some(ModelGenerationId { turn_id:"turn".into(), ordinal:1 });
    let mut state = ToolHistoryState::default();
    state.register(record);
    assert_eq!(state.candidates["validation"].artifact_pin_value().unwrap()["source_dependencies_current"], true);
    assert!(state.invalidate_source_dependencies(Some(&BTreeSet::from([source])), None));
    let pins = state.artifact_pin_payload_for_items(&[text_output("validation", text)]).unwrap();
    let pin: serde_json::Value = serde_json::from_str(&pins).unwrap();
    assert_eq!(pin["artifacts"][0]["successful"], true);
    assert_eq!(pin["artifacts"][0]["source_dependencies_current"], false);
    state.retain_for_history(&[text_output("compacted", pins)]);
    persist_tool_history_state(temp.path(), "thread", &state).await.unwrap();
    let restored = expect_loaded_tool_history(load_tool_history_state(temp.path(), "thread").await);
    let pin = restored.candidates["validation"].artifact_pin_value().unwrap();
    assert_eq!(pin["successful"], true);
    assert_eq!(pin["source_dependencies_current"], false);
    assert_eq!(pin["artifact_id"], state.candidates["validation"].artifact_id);
}

#[test]
fn receipt_digest_preserves_decisive_diagnostic_at_any_position() {
    for position in [11, 333, 880] {
        let mut lines = vec!["compiler boilerplate"; 1000];
        lines[position] = "error: decisive counterexample: alias overwrote source A";
        for output in [lines.join("\n"), serde_json::json!({"output":lines.join("\n"), "exit_code":1}).to_string()] {
            let mut record = candidate("failure", output);
            record.successful = false;
            record.refresh_derived();
            let receipt: serde_json::Value = serde_json::from_str(record.admission_receipt().unwrap().1).unwrap();
            assert!(receipt["digest"].as_str().unwrap().contains("decisive counterexample"));
            assert!(record.artifact_pin_value().unwrap()["digest"].as_str().unwrap().contains("decisive counterexample"));
        }
    }
    let record = candidate("selection", serde_json::json!({"output":"boilerplate".repeat(4000),
        "results":[{"selector":{"kind":"lines","start":1,"end":2},
            "status":"not_found", "message":"decisive selection error"}]}).to_string());
    assert!(record.artifact_pin_value().unwrap()["digest"].as_str().unwrap().contains("decisive selection error"));
}

#[test]
fn recovered_detail_preference_ends_after_turn_or_compact_exposure() {
    let mut state = ToolHistoryState::default();
    state.recovered_call_ids.insert("page".into());
    state.mark_consumed(&[text_output("page", "small recovered detail".into())],
        ModelGenerationId { turn_id:"first".into(), ordinal:1 });
    assert!(state.reuse_priority("page", 5) < state.reuse_priority("cold", 5));
    state.mark_consumed(&[text_output("next", "new investigation".into())],
        ModelGenerationId { turn_id:"second".into(), ordinal:1 });
    assert_eq!(state.reuse_priority("page", 5), state.reuse_priority("cold", 5));
    state.exposed_representations.insert("page".into(), ExposedRepresentation::Compact { sha256: "pin".into() });
    assert_eq!(state.reuse_priority("page", 5), state.reuse_priority("cold", 5));
}

#[test]
fn read_status_keeps_environments_and_legacy_observations_separate() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.rs");
    let mut state = ToolHistoryState::default();
    for (call, environment, start, end) in [
        ("one", Some("env-a"), 0, 5), ("two", Some("env-b"), 5, 10),
        ("legacy-one", None, 0, 5), ("legacy-two", None, 5, 10),
    ] {
        let value = serde_json::json!({"path":path, "source_sha256":"same", "canonical_bytes":10,
            "environment_id":environment, "canonical_uri":environment.map(|_| "file:///source.rs"),
            "results":[{"status":"ok", "complete":true,
                "canonical_range":{"start":start,"end":end}, "text":"x".repeat(end-start)}]});
        let mut entry = candidate(call, value.to_string());
        entry.tool_identity = "read_file".into();
        state.register(entry);
    }
    let report = state.read_status(&[path.clone()], None, &[]);
    let snapshots = report["paths"][0]["snapshots"].as_array().unwrap();
    assert_eq!(snapshots.len(), 4);
    assert!(snapshots.iter().all(|row| row["coverage"] == "partial" && row["obtained_bytes"] == 5));
    let filtered = state.read_status(&[path], Some("env-a"), &[]);
    assert!(filtered["paths"][0]["snapshots"].as_array().unwrap().iter()
        .all(|row| row["environment_id"] != "env-b"));
}

#[test]
fn read_status_merges_only_same_snapshot_and_retains_unknown_and_stale_states() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.rs");
    let mut state = ToolHistoryState::default();
    for (call, hash, start, end, current) in [
        ("first", "a", 0, 4, true), ("second", "a", 4, 8, true),
        ("old", "b", 0, 2, false),
    ] {
        let value = serde_json::json!({"path":path, "source_sha256":hash, "canonical_bytes":10,
            "environment_id":"local", "canonical_uri":"file:///source.rs",
            "artifact_id":format!("snapshot-{call}"), "retained_artifact_complete":true,
            "results":[{"status":"ok", "complete":true,
                "canonical_range":{"start":start, "end":end}, "text":"x".repeat(end-start)}]});
        let mut entry = candidate(call, value.to_string());
        entry.tool_identity = "read_file".into();
        entry.source_dependencies_current = current;
        state.register(entry);
    }
    state.recovered_ranges.insert("second".into(), vec![serde_json::json!({"kind":"bytes","start":8,"end":10})]);
    let persisted = serde_json::to_value(&state).unwrap();
    let restored: ToolHistoryState = serde_json::from_value(persisted.clone()).unwrap();
    let report = restored.read_status(&[path, root.path().join("unread.rs")], Some("local"), &[]);
    let snapshots = report["paths"][0]["snapshots"].as_array().unwrap();
    let full = snapshots.iter().find(|row| row["source_sha256"] == "a").unwrap();
    let partial = snapshots.iter().find(|row| row["source_sha256"] == "b").unwrap();
    assert_eq!(full["coverage"], "full");
    assert_eq!(full["obtained_bytes"], 10);
    assert_eq!(partial["coverage"], "partial");
    assert_eq!(partial["freshness"], "invalidated");
    assert_eq!(report["paths"][1]["status"], "unknown");
    assert_eq!(serde_json::to_value(restored).unwrap(), persisted, "read-only query");
}

#[test]
fn evidence_budget_reserves_generation_room_without_plan_authority() {
    let mut state = ToolHistoryState::default();
    let mut reusable = candidate("reusable", "retained source\n".repeat(100));
    reusable.consumed_by_generation = Some(ModelGenerationId {
        turn_id: "previous".into(), ordinal: 1,
    });
    state.register(reusable);
    let baseline = state.task_sensitive_tool_result_budget(100_000, 0, 4096);
    let pressured = state.task_sensitive_tool_result_budget(100_000, 90_000, 8192);
    assert!(pressured < baseline, "evidence must leave room for generation");
    assert!(pressured <= 100_000 - 8192);
    state.recovered_call_ids.insert("reusable".into());
    let reusable_budget = state.task_sensitive_tool_result_budget(100_000, 0, 4096);
    assert!(reusable_budget >= baseline);
    assert!(reusable_budget <= model_visible_tool_result_token_budget());
}

#[tokio::test]
async fn recovered_evidence_budget_is_identical_with_pending_completed_and_absent_plans() {
    use codex_protocol::plan_tool::PlanItemArg;
    use codex_protocol::plan_tool::StepStatus;
    use codex_protocol::plan_tool::UpdatePlanArgs;
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let mut state = ToolHistoryState::default();
    // The recovered detail and its originating artifact are distinct calls.
    // Recent recovery may prefer the detail, never promote its whole producer.
    let mut origin = candidate("origin", "canonical source".into());
    origin.consumed_by_generation = Some(ModelGenerationId { turn_id: "old".into(), ordinal: 1 });
    state.register(origin);
    state.consumption_turns.push("old".into());
    let mut evidence = candidate("recovered", "source ".repeat(20_000));
    evidence.consumed_by_generation = Some(ModelGenerationId { turn_id: "old".into(), ordinal: 1 });
    state.register(evidence);
    let baseline = state.task_sensitive_tool_result_budget(100_000, 0, 4096);
    state.recovered_call_ids.insert("recovered".into());
    let expected = state.task_sensitive_tool_result_budget(100_000, 0, 4096);
    assert!(expected > baseline, "fixture must exercise recovered evidence preference");
    {
        let mut owner = session.lock_history_state_for_test().await;
        owner.history.set_token_info(Some(codex_protocol::protocol::TokenUsageInfo {
            model_context_window: Some(100_000),
            total_token_usage: Default::default(),
            last_token_usage: Default::default(),
        }));
        owner.history.set_tool_history_state(state);
    }
    for status in [Some(StepStatus::Pending), Some(StepStatus::Completed), None] {
        session.services.plan_store.restore(status.map(|status| UpdatePlanArgs {
            explanation: None,
            plan: vec![PlanItemArg { step: "Investigate".into(), status }],
        })).await;
        assert_eq!(session.clone_history().await.tool_history_state()
            .configured_model_visible_tool_result_token_budget(), Some(expected));
    }
}

#[tokio::test]
async fn journal_writer_reuses_handles_and_replays_after_another_writer() {
    let temp = tempfile::tempdir().unwrap();
    let writer = Arc::new(std::sync::Mutex::new(ToolHistoryJournalWriter::default()));
    let mut state = ToolHistoryState::default();
    state.register_non_workspace_code_mode_call("initial".into());
    persist_tool_history_state_with_writer(temp.path(), "thread", &state, Arc::clone(&writer))
        .await.unwrap();
    assert_eq!(writer.lock().unwrap().checkpoint_replays, 1);
    for sequence in 1..=2 {
        let mutation = ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
            call_id: format!("local-{sequence}"),
        };
        mutation.apply(&mut state);
        persist_tool_history_mutations_with_writer(temp.path(), "thread", "local",
            &[(sequence, mutation)], Arc::clone(&writer)).await.unwrap();
    }
    assert_eq!(writer.lock().unwrap().journal_opens, 1);
    persist_tool_history_state_with_writer(temp.path(), "thread", &state, Arc::clone(&writer))
        .await.unwrap();
    assert_eq!(writer.lock().unwrap().checkpoint_replays, 1);
    let foreign = ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall { call_id: "foreign".into() };
    foreign.apply(&mut state);
    persist_tool_history_mutations(temp.path(), "thread", "foreign", &[(1, foreign)])
        .await.unwrap();
    persist_tool_history_state_with_writer(temp.path(), "thread", &state, Arc::clone(&writer))
        .await.unwrap();
    assert_eq!(writer.lock().unwrap().checkpoint_replays, 2);
    let ledger: ToolHistoryLedgerFile = serde_json::from_slice(
        &std::fs::read(ledger_path(temp.path(), "thread")).unwrap(),
    ).unwrap();
    assert_eq!(ledger.journal_sequences.get("local"), Some(&2));
    assert_eq!(ledger.journal_sequences.get("foreign"), Some(&1));
    assert!(ledger.state.non_workspace_code_mode_calls.contains("foreign"));
}

#[tokio::test]
async fn admission_read_only_proof_rejects_shell_writes_and_unknown_commands() {
    for (arguments, expected) in [
        (serde_json::json!({"program":"rg", "args":["needle", "."]}), true),
        (serde_json::json!({"program":"unknown-command", "args":[]}), false),
        (serde_json::json!({"cmd":"cat input > output", "shell":"bash"}), false),
        (serde_json::json!({"cmd":"cat input; rm output", "shell":"bash"}), false),
        (serde_json::json!({"cmd":"cat $(touch output)", "shell":"bash"}), false),
    ] {
        let (_, read_only) = classify_workspace_tool_call_at_admission(
            "exec_command".to_string(),
            ToolPayload::Function { arguments: arguments.to_string() },
            std::env::current_dir().unwrap(),
        ).await.unwrap();
        assert_eq!(read_only, expected, "{arguments}");
    }
}

/// Deterministic projection/recovery workload, not a model-latency benchmark.
#[tokio::test]
async fn long_session_prompt_pressure_comparison() {
    use crate::tools::command_output_artifact::ToolOutputSelector;

    let home = tempfile::tempdir().unwrap();
    let mut original = ToolHistoryState::default();
    let mut items = Vec::new();
    let source = bounded_output();
    for index in 0..18 {
        let id = format!("completed-{index}");
        let canonical = CanonicalToolResult::text(source.clone());
        let artifact = create_canonical_output_artifact(home.path(), "pressure", &canonical).await;
        let mut entry = candidate(&id, source.clone());
        entry.artifact_id = artifact.artifact_id().unwrap();
        entry.artifact_bytes = canonical.exact_bytes;
        entry.artifact_sha256 = canonical.sha256;
        entry.consumed_by_generation = Some(ModelGenerationId {
            turn_id: "pressure".into(),
            ordinal: index,
        });
        original.register(entry);
        original.register_non_workspace_code_mode_call(id.clone());
        items.extend([function_call(&id), text_output(&id, source.clone())]);
    }
    let active = "unresolved failure: preserve this active diagnostic\n".repeat(20);
    let mut entry = candidate("active", active.clone());
    entry.successful = false;
    original.register(entry);
    items.extend([
        function_call("active"),
        text_output("active", active.clone()),
    ]);
    let receipts = original
        .phase_checkpoint_receipts(&["completed-0".into()])
        .unwrap();
    items.push(ResponseItem::Message {
        id: None,
        role: "developer".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: format!(
                "<completed_phase_checkpoint>\n{}\n</completed_phase_checkpoint>",
                serde_json::json!({"receipts":receipts})
            ),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    });
    let items: Arc<[ResponseItem]> = items.into();
    let mut reports = Vec::new();
    for budget in [129_000, 96_000, 75_000, 4_000] {
        let started = std::time::Instant::now();
        let mut state = original.clone();
        state.set_model_visible_tool_result_token_budget(Some(budget));
        let projection = state.project(Arc::clone(&items));
        let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
        let sampling =
            state.project_sampling_with_workspace_cache(Arc::clone(&items), None, &cache);
        let tokens = |items: &[ResponseItem]| {
            items
                .iter()
                .filter_map(canonical_textual_output_identity)
                .map(|(_, text)| approx_token_count(&text))
                .sum::<usize>()
        };
        let mut session_input_tool_tokens = 0;
        for count in 1..=18 {
            let mut request = items[..count * 2].to_vec();
            request.extend_from_slice(&items[36..]);
            let request = state.project_sampling_with_workspace_cache(request.into(), None, &cache);
            session_input_tool_tokens += tokens(&request.items);
        }
        assert!(
            projection
                .items
                .contains(&text_output("active", active.clone())),
            "budget={budget}"
        );
        let mut compact_receipt_tokens = 0;
        let mut recovery_calls = 0;
        for item in projection.items.iter() {
            let Some((id, text)) = canonical_textual_output_identity(item) else {
                continue;
            };
            if id == "active" || text == source {
                continue;
            }
            compact_receipt_tokens += approx_token_count(&text);
        }
        // Revisit a fixed working set, recovering omitted evidence from the
        // artifact owner instead of executing any producer again.
        for index in 1..18 {
            let id = format!("completed-{index}");
            if projection.items.contains(&text_output(&id, source.clone())) {
                continue;
            }
            let artifact = &state.candidates[&id].artifact_id;
            let (recovered, _) = crate::tools::handlers::execute_recovery_transaction(
                home.path(),
                "pressure",
                artifact,
                vec![ToolOutputSelector::Lines { start: 1, end: 2 }],
                false,
            )
            .await
            .unwrap();
            recovery_calls += 1;
            assert!(recovered.complete);
            assert_eq!(
                recovered.results[0]
                    .text
                    .as_deref()
                    .unwrap()
                    .lines()
                    .collect::<Vec<_>>(),
                source.lines().take(2).collect::<Vec<_>>()
            );
        }
        assert!(tokens(&projection.items) <= budget);
        let report = serde_json::json!({
            "budget": budget,
            "model_input_tool_tokens": tokens(&sampling.items),
            "session_input_tool_tokens": session_input_tool_tokens,
            "compaction_input_tool_tokens": tokens(&projection.items),
            "compact_receipt_tokens": compact_receipt_tokens,
            "read_tool_output_recovery_calls": recovery_calls,
            "producer_results": original.candidates.len() - 1,
            "completion_ms": started.elapsed().as_secs_f64() * 1000.0,
        });
        eprintln!("tool-history-pressure {report}");
        reports.push(report);
    }
    assert!(
        reports[3]["read_tool_output_recovery_calls"]
            .as_u64()
            .unwrap()
            > 0
    );
    // Ordinary sampling preserves evidence until explicit checkpointing;
    // changing the compaction working set alone is not a sampling speedup.
    assert_eq!(
        reports[0]["model_input_tool_tokens"],
        reports[2]["model_input_tool_tokens"]
    );
    assert!(
        reports[0]["read_tool_output_recovery_calls"]
            .as_u64()
            .unwrap()
            <= reports[2]["read_tool_output_recovery_calls"]
                .as_u64()
                .unwrap()
    );

    // The next task uses the real sampling owner, including restart recovery.
    // A final answer plus review-to-fix continuation is not a checkpoint. The
    // same evidence stays directly usable without manufactured recovery calls.
    persist_tool_history_state(home.path(), "pressure", &original).await.unwrap();
    let restored = expect_loaded_tool_history(load_tool_history_state(home.path(), "pressure").await);
    let mut history = crate::context_manager::ContextManager::new();
    history.set_tool_history_state(restored);
    let message = |role: &str, text: &str, phase| ResponseItem::Message {
        id: None, role: role.into(),
        content: vec![codex_protocol::models::ContentItem::InputText { text: text.into() }],
        phase, internal_chat_message_metadata_passthrough: None,
    };
    history.record_items(items.iter(), TruncationPolicy::Tokens(100_000));
    history.record_items([
        &message("assistant", "The source review is complete; the failure remains unresolved.", Some(codex_protocol::models::MessagePhase::FinalAnswer)),
        &message("user", "Use the review to fix the failure.", None),
    ], TruncationPolicy::Tokens(100_000));
    let canonical = history.raw_items().to_vec();
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let sampled = history.clone().prepare_for_sampling_prompt_with_completed_tool_projection(
        &[codex_protocol::openai_models::InputModality::Text],
        crate::stable_context::StableContextTarget::Sampling, None, &cache,
    );
    assert!(sampled.items().contains(&text_output("active", active)));
    let continuation_started = std::time::Instant::now();
    let mut continuation_recovery_calls = 0;
    for index in 0..18 {
        let id = format!("completed-{index}");
        let (_, text) = sampled.items().iter().filter_map(canonical_textual_output_identity)
            .find(|(call_id, _)| *call_id == id).unwrap();
        if index != 0 {
            assert_eq!(text, source, "review-to-implementation needs no recovery");
            continue;
        }
        // Only the explicitly checkpointed result was retired. Keep its exact
        // recovery proof without reconstructing the other seventeen results.
        let pin: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(pin["kind"], "tool_history_artifact_pin");
        let artifact_id = &original.candidates[&id].artifact_id;
        assert_eq!(read_exact_tool_output_artifact(
            home.path(), "pressure", artifact_id,
        ).await.unwrap(), source.as_bytes());
        let (recovered, _) = crate::tools::handlers::execute_recovery_transaction(
            home.path(), "pressure", artifact_id,
            vec![ToolOutputSelector::Lines { start: 1, end: 2 }], false,
        ).await.unwrap();
        continuation_recovery_calls += 1;
        assert!(recovered.complete);
        assert_eq!(recovered.results[0].text.as_deref().unwrap().lines().collect::<Vec<_>>(),
            source.lines().take(2).collect::<Vec<_>>());
    }
    let tokens = |items: &[ResponseItem]| items.iter().filter_map(canonical_textual_output_identity)
        .map(|(_, text)| codex_utils_output_truncation::model_token_count(&text)).sum::<usize>();
    assert_eq!(continuation_recovery_calls, 1);
    assert!(tokens(sampled.items()) < tokens(&canonical));
    assert_eq!(history.raw_items(), canonical, "canonical evidence remains unchanged");
    eprintln!("review-to-implementation continuation_ms={} actual_model_requests=0 recovery_calls={continuation_recovery_calls} input_tool_tokens_before={} after={} live_model_correctness=unmeasured",
        continuation_started.elapsed().as_secs_f64() * 1000.0, tokens(&canonical), tokens(sampled.items()));
}

#[test]
fn task_sensitive_budget_reserves_unread_evidence_and_releases_it_after_consumption() {
    let mut state = ToolHistoryState::default();
    let evidence = candidate("active-budget", bounded_output().repeat(2));
    state.register(evidence.clone());
    let mut history = crate::context_manager::ContextManager::new();
    history.update_token_info(&codex_protocol::protocol::TokenUsage::default(), Some(40_000));
    history.set_tool_history_state(state);
    let budget = history.tool_history_state().configured_model_visible_tool_result_token_budget().unwrap();
    assert!(budget > 20_000 && budget <= 30_000, "{budget}");
    history.mark_tool_history_consumed_with_delta(
        &[text_output("active-budget", evidence.bounded_model_output)],
        ModelGenerationId { turn_id: "task".into(), ordinal: 0 },
    );
    assert_eq!(history.tool_history_state().configured_model_visible_tool_result_token_budget(), Some(20_000));
}

#[test]
fn checkpoint_does_not_grant_contract_provenance_to_printed_text() {
    let mut state = ToolHistoryState::default();
    for id in ["ordinary", "lookup", "source-marker", "unread-schema"] {
        let mut entry = candidate(id, bounded_output());
        entry.artifact_id = format!("artifact-{id}");
        entry.consumed_by_generation = Some(ModelGenerationId {
            turn_id: "turn".into(), ordinal: 1,
        });
        if id != "ordinary" {
            entry.tool_identity = "functions.exec".into();
            entry.bounded_model_output.push_str(&serde_json::json!({
                "name":"sample", "description":"exec tool declaration:\n```ts\ndeclare const tools: { sample(): void };\n```"
            }).to_string());
        }
        if id == "unread-schema" {
            entry.consumed_by_generation = None;
        }
        if id == "source-marker" {
            entry.bounded_model_output.push_str("\n// source containing exec tool declaration:\n");
        }
        state.register(entry);
    }
    assert!(state.candidates["ordinary"].checkpoint_pin().is_some());
    assert!(state.candidates["lookup"].checkpoint_pin().is_some());
    assert!(state.candidates["source-marker"].checkpoint_pin().is_some());
    assert!(state.candidates["unread-schema"].checkpoint_pin().is_none());
    assert_ne!(state.phase_checkpoint_receipts(&["lookup".into()]).unwrap(), serde_json::json!({}));
    assert!(state.candidates["lookup"].bounded_model_output.contains("declare const tools"));
}

#[test]
fn consumed_failure_keeps_receipt_without_displacing_corrected_evidence() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(3_000);
    let mut state = ToolHistoryState::default();
    let mut failed = candidate("bad-selector", "invalid selector diagnostic\n".repeat(4_000));
    failed.successful = false;
    failed.consumed_by_generation = Some(ModelGenerationId { turn_id: "turn".into(), ordinal: 1 });
    let artifact = failed.artifact_id.clone();
    let old = failed.bounded_model_output.clone();
    state.register(failed);
    let current = "required corrected selection\n".repeat(300);
    state.register(candidate("correct-selector", current.clone()));
    let projected = state.project(Arc::from([
        function_call("bad-selector"), text_output("bad-selector", old),
        function_call("correct-selector"), text_output("correct-selector", current.clone()),
    ]));
    let outputs = projected.items.iter().filter_map(textual_output_identity).collect::<BTreeMap<_, _>>();
    assert_eq!(outputs["correct-selector"], current);
    let receipt: serde_json::Value = serde_json::from_str(outputs["bad-selector"]).unwrap();
    assert_eq!(receipt["artifact_id"], artifact);
    assert_eq!(receipt["successful"], false);
    assert!(state.failure_resolution(&state.candidates["bad-selector"], &FailureResolutionIndex::default()).is_none());
}

#[test]
fn phase_checkpoint_leaves_tiny_results_inline() {
    let mut state = ToolHistoryState::default();
    for text in ["ok".to_string(), "x".repeat(900), " ".repeat(2_000)] {
        let mut tiny = candidate("tiny", text);
        tiny.consumed_by_generation = Some(ModelGenerationId {
            turn_id: "turn".into(),
            ordinal: 0,
        });
        state.register(tiny);
        assert_eq!(
            state.phase_checkpoint_receipts(&["tiny".into()]).unwrap(),
            serde_json::json!({})
        );
    }
}


#[test]
fn phase_checkpoint_compacts_only_selected_consumed_recoverable_evidence() {
    let mut state = ToolHistoryState::default();
    let source = bounded_output();
    let mut done = candidate("done", source.clone());
    done.consumed_by_generation = Some(ModelGenerationId {
        turn_id: "phase-one".into(),
        ordinal: 0,
    });
    state.register(done);
    state.register(candidate("active", source.clone()));
    state.register_non_workspace_code_mode_call("done".into());
    state.register_non_workspace_code_mode_call("active".into());
    assert!(state.phase_checkpoint_receipts(&["active".into()]).is_err());
    let diagnostic: serde_json::Value = serde_json::from_str(
        &state.phase_checkpoint_receipts(&["missing".into(), "also-missing".into()]).unwrap_err(),
    ).unwrap();
    assert_eq!(diagnostic["unknown_call_ids"], serde_json::json!(["missing", "also-missing"]));
    assert_eq!(diagnostic["eligible_call_ids"], serde_json::json!(["done"]));
    let receipts = state.phase_checkpoint_receipts(&["done".into()]).unwrap();
    assert!(receipts["done"].get("digest").is_none());
    let checkpoint = ResponseItem::Message {
        id: None,
        role: "developer".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: format!(
                "<completed_phase_checkpoint>\n{}\n</completed_phase_checkpoint>",
                serde_json::json!({"receipts":receipts,"active_work":"keep active source"})
            ),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let items: Arc<[ResponseItem]> = Arc::from([
        function_call("done"),
        text_output("done", source.clone()),
        function_call("active"),
        text_output("active", source.clone()),
        checkpoint.clone(),
    ]);
    let projection = state.project_inner(Arc::clone(&items), None, None);
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let before: Arc<[ResponseItem]> = Arc::from(items[..items.len() - 1].to_vec());
    let anchor = SamplingProjectionAnchor {
        projection: state.project_inner(Arc::clone(&before), None, None),
        prepared_items: before,
    };
    assert!(
        state
            .project_continuation_with_workspace_cache(&anchor, Arc::clone(&items), None, &cache)
            .is_none(),
        "a newly appended checkpoint must rebuild the cached projection"
    );
    let anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&items),
        projection: projection.clone(),
    };
    assert_eq!(
        state
            .project_continuation_with_workspace_cache(&anchor, Arc::clone(&items), None, &cache)
            .unwrap()
            .items,
        projection.items,
        "an existing checkpoint must preserve the newly cached prefix"
    );
    let done = projection
        .items
        .iter()
        .filter_map(canonical_textual_output_identity)
        .find(|(id, _)| *id == "done")
        .unwrap()
        .1;
    assert!(done.contains("tool_history_artifact_pin"));
    assert!(done.len() < source.len() / 10);
    assert!(
        projection
            .items
            .contains(&text_output("active", source.clone()))
    );
    assert!(projection.items.contains(&checkpoint));
    assert!(items.contains(&text_output("done", source)));
}

#[test]
fn phase_checkpoint_resolves_only_later_current_success_for_the_same_action() {
    let mut state = ToolHistoryState::default();
    let source = bounded_output();
    let action = sha256(b"test exact target");
    let mut failure = candidate("failed", source.clone());
    failure.successful = false;
    failure.supersession_identity = Some(format!("functions.exec:{action}:{}", sha256(b"failed")));
    failure.consumed_by_generation = Some(ModelGenerationId { turn_id: "turn".into(), ordinal: 0 });
    state.register(failure);
    assert!(state.phase_checkpoint_receipts(&["failed".into()]).is_err());
    let mut success = candidate("repaired", source.clone());
    success.supersession_identity = Some(format!("functions.exec:{action}:{}", sha256(b"passed")));
    success.consumed_by_generation = Some(ModelGenerationId { turn_id: "turn".into(), ordinal: 1 });
    state.register(success.clone());
    let receipts = state.phase_checkpoint_receipts(&["failed".into()]).unwrap();
    assert_eq!(receipts["failed"]["resolved_by"]["call_id"], "repaired");
    let checkpoint = ResponseItem::Message {
        id: None, role: "developer".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: format!("<completed_phase_checkpoint>\n{}\n</completed_phase_checkpoint>",
                serde_json::json!({"receipts": receipts})),
        }],
        phase: None, internal_chat_message_metadata_passthrough: None,
    };
    let items: Arc<[ResponseItem]> = Arc::from([
        function_call("failed"), text_output("failed", source.clone()), checkpoint,
    ]);
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let projected = state.project_sampling_with_workspace_cache(Arc::clone(&items), None, &cache);
    assert!(!projected.items.contains(&text_output("failed", source.clone())));
    success.source_dependencies_current = false;
    state.register(success.clone());
    assert!(state.phase_checkpoint_receipts(&["failed".into()]).is_err());
    let projected = state.project_sampling_with_workspace_cache(items, None, &cache);
    assert!(projected.items.contains(&text_output("failed", source)));
    success.source_dependencies_current = true;
    success.supersession_identity = Some(format!("functions.exec:{}:{}", sha256(b"other target"), sha256(b"passed")));
    state.register(success);
    assert!(state.phase_checkpoint_receipts(&["failed".into()]).is_err());
}

#[test]
fn checkpoint_answer_lineage_downgrades_without_rewriting_or_repeating_notes() {
    let mut state = ToolHistoryState::default();
    let mut evidence = candidate("source", bounded_output());
    state.register(evidence.clone());
    let checkpoint = ResponseItem::Message {
        id: None, role: "developer".into(),
        content: vec![codex_protocol::models::ContentItem::InputText {
            text: format!("<completed_phase_checkpoint>\n{}\n</completed_phase_checkpoint>",
                serde_json::json!({"receipts": {}, "answered_questions": [{
                    "question": "What is the source?", "answer": "A prior observation.",
                    "evidence_refs": ["source"],
                }]})),
        }],
        phase: None, internal_chat_message_metadata_passthrough: None,
    };
    let items: Arc<[ResponseItem]> = Arc::from([checkpoint.clone()]);
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let current = state.project_sampling_with_workspace_cache(Arc::clone(&items), None, &cache);
    assert_eq!(current.items.as_ref(), items.as_ref());
    evidence.source_dependencies_current = false;
    state.register(evidence);
    let stale = state.project_sampling_with_workspace_cache(items, None, &cache);
    assert_eq!(stale.items.len(), 2);
    assert_eq!(stale.items[0], checkpoint);
    let notice = serde_json::to_value(&stale.items[1]).unwrap().to_string();
    assert!(notice.contains("checkpoint_answer_evidence"));
    assert!(notice.contains("unverified"));
    let repeated = state.project_sampling_with_workspace_cache(Arc::clone(&stale.items), None, &cache);
    assert_eq!(repeated.items, stale.items);
}

#[test]
fn verified_evidence_uncertainty_only_lineage_downgrades_both_reference_sets() {
    for field in ["supporting_evidence", "contradicting_evidence"] {
        let mut state = ToolHistoryState::default();
        let mut stale = candidate("stale", bounded_output());
        stale.source_dependencies_current = false;
        state.register(stale);
        state.register(candidate("current", bounded_output()));
        let mut uncertainty = serde_json::json!({"claim":"open", "next_action":"inspect"});
        uncertainty[field] = serde_json::json!(["stale", "current"]);
        let checkpoint = ResponseItem::Message {
            id: None, role: "developer".into(),
            content: vec![codex_protocol::models::ContentItem::InputText {
                text: format!("<completed_phase_checkpoint>\n{}\n</completed_phase_checkpoint>",
                    serde_json::json!({"receipts":{}, "uncertainties":[uncertainty]})),
            }], phase: None, internal_chat_message_metadata_passthrough: None,
        };
        let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
        let projection = state.project_sampling_with_workspace_cache(Arc::from([checkpoint.clone()]), None, &cache);
        assert_eq!(projection.items.len(), 2);
        assert_eq!(projection.items[0], checkpoint);
        let ResponseItem::Message { content, .. } = &projection.items[1] else { panic!("notice") };
        let codex_protocol::models::ContentItem::InputText { text } = &content[0] else { panic!("text") };
        let notice: serde_json::Value = text.lines().find_map(|line| serde_json::from_str(line).ok()).unwrap();
        assert_eq!(notice["notices"][0]["kind"], "checkpoint_uncertainty_evidence");
        assert_eq!(notice["notices"][0]["evidence_refs"], serde_json::json!(["stale"]));
    }
}

#[test]
fn sampling_freshness_appends_invalidations_without_rewriting_or_repeating_history() {
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let captured = workspace_identity("captured");
    let changed = workspace_identity("changed");
    let later = workspace_identity("later-unrelated-edit");
    let output = text_output("read-old", "original source evidence".into());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call("read-old"), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured.clone()),
            &output,
            BTreeSet::from([SourceDependencyV1::new(std::path::Path::new("source.rs"), false)]),
        )
        .unwrap(),
    );
    let initial = state.project_sampling_with_workspace_cache(
        Arc::clone(&canonical),
        Some(&captured),
        &cache,
    );
    assert_eq!(initial.items, canonical);
    let anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: initial.clone(),
    };
    let invalidated = state
        .project_continuation_with_workspace_cache(
            &anchor,
            Arc::clone(&canonical),
            Some(&changed),
            &cache,
        )
        .unwrap();
    for items in [&invalidated.items, &invalidated.unreplaced_items] {
        assert!(
            items.starts_with(&initial.items),
            "previously delivered source bytes must remain identical"
        );
        assert_eq!(items.len(), initial.items.len() + 1);
        let ResponseItem::Message { role, content, .. } = items.last().unwrap() else {
            panic!("invalidation message")
        };
        assert_eq!(role, "developer");
        let text = serde_json::to_string(content).unwrap();
        assert!(text.contains("read-old"));
        assert!(text.contains("stale_workspace_evidence"));
        assert!(
            !text.contains("original source evidence"),
            "do not duplicate old output in a higher-trust message"
        );
    }
    let anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: invalidated.clone(),
    };
    let repeated = state
        .project_continuation_with_workspace_cache(
            &anchor,
            Arc::clone(&canonical),
            Some(&later),
            &cache,
        )
        .unwrap();
    assert_eq!(
        repeated.items, invalidated.items,
        "unrelated workspace revisions must not repeat the warning"
    );

    let new_output = text_output("read-new", "current source evidence".into());
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(later.clone()),
            &new_output,
            BTreeSet::from([SourceDependencyV1::new(std::path::Path::new("source.rs"), false)]),
        )
        .unwrap(),
    );
    let mut extended = canonical.to_vec();
    extended.extend([function_call("read-new"), new_output.clone()]);
    let next = state
        .project_continuation_with_workspace_cache(&anchor, extended.into(), Some(&later), &cache)
        .unwrap();
    assert!(next.items.starts_with(&invalidated.items));
    assert_eq!(
        next.items.last(),
        Some(&new_output),
        "fresh replacement evidence must remain current"
    );
    assert_eq!(next.items.len(), invalidated.items.len() + 2);

    let without_budget = state.project_workspace_freshness_with_cache(
        Arc::clone(&canonical),
        Some(&changed),
        &cache,
    );
    assert!(without_budget.items.starts_with(&canonical));
    assert_eq!(without_budget.items.len(), canonical.len() + 1);
}

#[test]
fn child_message_continuation_preserves_the_previous_freshness_layout() {
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let captured = workspace_identity("captured");
    let changed = workspace_identity("changed");
    let output = text_output("read-old", "original source evidence".into());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call("read-old"), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured),
            &output,
            BTreeSet::new(),
        )
        .unwrap(),
    );
    let previous = state.project_sampling_with_workspace_cache(
        Arc::clone(&canonical), Some(&changed), &cache,
    );
    assert_eq!(previous.items.len(), canonical.len() + 1, "fixture has a freshness notice");
    let anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: previous.clone(),
    };
    let mail = codex_protocol::protocol::InterAgentCommunication::new(
        codex_protocol::AgentPath::try_from("/root/worker").unwrap(),
        codex_protocol::AgentPath::root(),
        Vec::new(),
        "child result".to_string(),
        true,
    );
    // Cover both current agent messages and the legacy assistant encoding.
    let legacy: ResponseItem = mail.to_response_input_item().into();
    for message in [mail.to_model_input_item(), legacy] {
        let mut extended = canonical.to_vec();
        extended.push(message.clone());
        let continued = state.project_continuation_with_workspace_cache(
            &anchor, extended.into(), Some(&changed), &cache,
        ).expect("child messages continue the same history layout");
        for (items, prior) in [
            (&continued.items, &previous.items),
            (&continued.unreplaced_items, &previous.unreplaced_items),
        ] {
            assert!(items.starts_with(prior));
            assert_eq!(items.len(), prior.len() + 1, "notice must not move or repeat");
            assert_eq!(items.last(), Some(&message));
        }
    }
    // Actual user input and explicit checkpoints must still rebuild.
    for (role, text) in [
        ("user", "next task"),
        ("developer", "<completed_phase_checkpoint>\n{\"receipts\":{}}\n</completed_phase_checkpoint>"),
    ] {
        let mut extended = canonical.to_vec();
        extended.push(ResponseItem::Message {
            id: None,
            role: role.to_string(),
            content: vec![codex_protocol::models::ContentItem::InputText { text: text.to_string() }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        });
        assert!(state.project_continuation_with_workspace_cache(
            &anchor, extended.into(), Some(&changed), &cache,
        ).is_none());
    }
}

#[test]
fn sampling_freshness_batches_results_and_only_appends_new_invalidations() {
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let captured = workspace_identity("captured");
    let changed = workspace_identity("changed");
    let mut state = ToolHistoryState::default();
    let mut canonical = Vec::new();
    for id in ["first", "second"] {
        let output = text_output(id, format!("source bytes for {id}"));
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(
                Some(captured.clone()),
                &output,
                BTreeSet::new(),
            )
            .unwrap(),
        );
        canonical.extend([function_call(id), output]);
    }
    let canonical: Arc<[ResponseItem]> = canonical.into();
    let projected =
        state.project_sampling_with_workspace_cache(Arc::clone(&canonical), Some(&changed), &cache);
    assert!(projected.items.starts_with(&canonical));
    assert_eq!(projected.items.len(), canonical.len() + 1);
    let ResponseItem::Message { content, .. } = projected.items.last().unwrap() else {
        panic!("batch")
    };
    let codex_protocol::models::ContentItem::InputText { text } = &content[0] else {
        panic!("text")
    };
    let batch: serde_json::Value = text
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .unwrap();
    assert_eq!(text.lines().count(), 4, "one safety qualification precedes the batched evidence records");
    assert!(text.contains("This is not a request to rerun tests or builds."));
    let notices = batch["notices"].as_array().unwrap();
    assert_eq!(notices.len(), 2);
    assert_eq!(notices[0]["call_id"], "first");
    assert_eq!(notices[1]["call_id"], "second");
    assert!(
        notices
            .iter()
            .all(|notice| notice["rerun"]["instruction"].is_null())
    );
    let anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: projected.clone(),
    };
    let repeated = state
        .project_continuation_with_workspace_cache(&anchor, canonical, Some(&changed), &cache)
        .unwrap();
    assert_eq!(repeated.items, projected.items);
}

#[test]
fn invalidation_reason_changes_do_not_repeat_the_same_evidence_but_nested_changes_do() {
    let previous = serde_json::json!({
        "call_id": "read", "observed_revision": {"worktree_identity": "captured"},
        "stale_workspace_evidence": true, "valid_for_current_workspace": false,
        "reason": "repository changed", "reason_code": "workspace_identity_changed",
        "current_nested_results": [{"call_id": "still-current"}],
    });
    let mut next = previous.clone();
    next["reason"] = "source dependency changed".into();
    next["reason_code"] = "source_dependencies_invalidated".into();
    assert!(same_workspace_invalidation(&previous, &next));
    assert!(same_workspace_invalidation(&next, &previous));
    next["current_nested_results"] = serde_json::json!([]);
    assert!(!same_workspace_invalidation(&previous, &next));
    next = previous.clone();
    next["observed_revision"]["worktree_identity"] = "new observation".into();
    assert!(!same_workspace_invalidation(&previous, &next));
    next = previous.clone();
    next["call_id"] = "other".into();
    assert!(!same_workspace_invalidation(&previous, &next));
    next = previous.clone();
    next["valid_for_current_workspace"] = true.into();
    assert!(!same_workspace_invalidation(&previous, &next));
    assert!(!same_workspace_invalidation(&serde_json::json!({"reason":"one"}),
                                       &serde_json::json!({"reason":"two"})));
}

#[test]
fn stale_receipt_keeps_a_bounded_historical_digest() {
    let mut tracked = candidate("historical-receipt", "old finding: ".repeat(2000));
    tracked.source_dependencies_current = false;
    tracked.refresh_derived();
    let rendered = tracked.derived.receipt.as_ref().expect("stale receipt");
    let receipt: ToolHistoryReceiptV2 = serde_json::from_str(rendered).unwrap();
    assert!(!receipt.source_dependencies_current);
    assert!(
        receipt
            .digest
            .starts_with("STALE: historical snapshot only;")
    );
    assert!(receipt.digest.contains("old finding:"));
    assert!(approx_token_count(rendered) <= RECEIPT_MAX_TOKENS);
}

#[test]
fn stale_workspace_projection_keeps_only_authenticated_historical_text() {
    let call_id = "historical-evidence";
    let captured = workspace_identity("captured");
    let changed = workspace_identity("changed");
    let original = "old finding: ".repeat(2000);
    let output = text_output(call_id, original.clone());
    for (record_observation, visible_output, expect_digest) in [
        (true, original.clone(), true),
        (false, original, false),
        (true, "tampered output".to_string(), false),
    ] {
        let mut state = ToolHistoryState::default();
        if record_observation {
            state.register_workspace_evidence(
                WorkspaceEvidenceObservation::from_response_item(
                    Some(captured.clone()),
                    &output,
                    BTreeSet::new(),
                )
                .unwrap(),
            );
        }
        let projection = state.project_with_workspace_identity(
            Arc::from([function_call(call_id), text_output(call_id, visible_output)]),
            Some(&changed),
        );
        let (_, text) = textual_output_identity(&projection.items[1]).unwrap();
        let notice: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(notice["valid_for_current_workspace"], false);
        assert_eq!(notice["stale_workspace_evidence"], true);
        assert_eq!(notice.get("historical_digest").is_some(), expect_digest);
        if expect_digest {
            let digest = notice["historical_digest"].as_str().unwrap();
            assert!(digest.contains("old finding:"));
            assert!(approx_token_count(digest) <= RECEIPT_DIGEST_TARGET_TOKENS);
        }
    }
}

#[test]
fn read_file_workspace_evidence_tracks_paths_and_preserves_skill_reads() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path();
    let file = cwd.join("a file.txt");
    let arguments = serde_json::json!({"path": "a file.txt", "selectors": [{"kind": "lines", "start": 1, "end": 1}]});
    let payload = ToolPayload::Function {
        arguments: arguments.to_string(),
    };
    let classification = classify_workspace_tool_call("read_file", &payload, cwd);
    assert!(classification.observes_workspace);
    assert_eq!(
        classification.source_dependencies,
        BTreeSet::from([SourceDependencyV1::new(&file, false)])
    );
    let call_id = "file-read";
    let output = text_output(call_id, "old file text".to_string());
    let canonical: Arc<[ResponseItem]> = Arc::from([
        named_function_call_with_arguments(call_id, "read_file", arguments),
        output.clone(),
    ]);
    let captured = workspace_identity("captured");
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured.clone()),
            &output,
            classification.source_dependencies,
        )
        .unwrap(),
    );
    assert_eq!(
        state
            .project_with_workspace_identity(Arc::clone(&canonical), Some(&captured))
            .items,
        canonical
    );
    state.invalidate_source_dependencies(
        Some(&BTreeSet::from([cwd.join("other.txt")])),
        Some(&captured),
    );
    assert_eq!(
        state
            .project_with_workspace_identity(Arc::clone(&canonical), Some(&captured))
            .items,
        canonical
    );
    state.invalidate_source_dependencies(Some(&BTreeSet::from([file.clone()])), Some(&captured));
    let stale = state.project_with_workspace_identity(canonical, Some(&captured));
    let (_, text) = textual_output_identity(&stale.items[1]).unwrap();
    let notice: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(notice["reason_code"], "source_dependencies_invalidated");
    assert_eq!(notice["rerun"]["tool"], "read_file");
    assert!(notice["rerun"].get("force_fresh").is_none());

    let uri = codex_utils_path_uri::PathUri::from_host_native_path(&file).unwrap();
    let uri_payload = ToolPayload::Function {
        arguments: serde_json::json!({"path": uri.inferred_native_path_string()}).to_string(),
    };
    assert_eq!(
        source_dependencies_for_tool_call("read_file", &uri_payload, cwd),
        BTreeSet::from([SourceDependencyV1::new(&file, false)])
    );
    let remote_payload = ToolPayload::Function {
        arguments: serde_json::json!({"path": "a file.txt", "environment_id": "remote"})
            .to_string(),
    };
    let remote = classify_workspace_tool_call("read_file", &remote_payload, cwd);
    assert!(remote.observes_workspace);
    assert!(remote.source_dependencies.is_empty());
    let skill_args = serde_json::json!({"path": "skill:example"});
    let skill_payload = ToolPayload::Function {
        arguments: skill_args.to_string(),
    };
    assert!(!classify_workspace_tool_call("read_file", &skill_payload, cwd).observes_workspace);
    let skill_history: Arc<[ResponseItem]> = Arc::from([
        named_function_call_with_arguments("skill-read", "read_file", skill_args),
        text_output("skill-read", "skill instructions".to_string()),
    ]);
    assert_eq!(
        state
            .project_with_workspace_identity(
                Arc::clone(&skill_history),
                Some(&workspace_identity("changed"))
            )
            .items,
        skill_history
    );
}

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
fn source_dependency_normalization_uses_the_target_platform_case_policy() {
    let upper = normalized_source_path(Path::new("src/Owner.rs"));
    let lower = normalized_source_path(Path::new("src/owner.rs"));

    if cfg!(windows) {
        assert_eq!(upper, lower);
    } else {
        assert_ne!(upper, lower);
    }
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
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection,
    };
    let continued = ToolHistoryState::default()
        .project_continuation_with_workspace_cache(&anchor, Arc::clone(&canonical), None, &cache)
        .expect("unchanged history is a continuation");
    assert!(Arc::ptr_eq(&continued.items, &canonical));
    assert!(Arc::ptr_eq(
        &continued.substitutions,
        &anchor.projection.substitutions
    ));
}

#[test]
fn projection_retain_visits_each_item_once_in_order() {
    let canonical: Arc<[ResponseItem]> = Arc::from([
        text_output("first", "one".to_string()),
        text_output("second", "two".to_string()),
        text_output("third", "three".to_string()),
        text_output("fourth", "four".to_string()),
    ]);
    for (retain, expected) in [
        ([true, false, true, false], vec!["first", "third"]),
        ([false, true, true, true], vec!["second", "third", "fourth"]),
        ([true; 4], vec!["first", "second", "third", "fourth"]),
        ([false; 4], vec![]),
    ] {
        for mut items in [
            ProjectedResponseItems::Shared(Arc::clone(&canonical)),
            ProjectedResponseItems::Owned(canonical.to_vec()),
        ] {
            let was_shared = matches!(items, ProjectedResponseItems::Shared(_));
            let mut visited = Vec::new();
            items.retain(|item| {
                visited.push(item_call_id(item).unwrap().to_string());
                retain[visited.len() - 1]
            });

            assert_eq!(visited, ["first", "second", "third", "fourth"]);
            let items = items.into_shared();
            assert_eq!(
                items
                    .iter()
                    .map(|item| item_call_id(item).unwrap())
                    .collect::<Vec<_>>(),
                expected
            );
            if was_shared && retain == [true; 4] {
                assert!(Arc::ptr_eq(&items, &canonical));
            }
        }
    }
}
use crate::tools::command_execution::CommandAttemptKey;
use crate::tools::command_execution::CommandExecutionLedger;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::command_output_artifact::protect_active_tool_history_artifact;
use crate::tools::command_output_artifact::read_exact_tool_output_artifact;
use crate::tools::command_output_artifact::remint_tool_history_artifact_for_thread;
use crate::tools::handlers::command_search::RgSearchBreadth;
use crate::tools::handlers::command_search::RgSearchNarrowing;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_tools::CanonicalToolResult;
use codex_utils_output_truncation::TruncationPolicy;
use pretty_assertions::assert_eq;

fn bounded_output() -> String {
    "bounded model-visible tool output with enough material for a smaller receipt\n".repeat(700)
}

fn candidate(call_id: &str, bounded_model_output: String) -> ToolHistoryCandidate {
    let mut candidate = ToolHistoryCandidate {
        call_id: call_id.to_string(),
        tool_identity: "functions.exec".to_string(),
        semantic_class: "tool_output".to_string(),
        successful: true,
        source_dependencies: BTreeSet::new(),
        source_dependencies_current: true,
        artifact_id: "artifact-1".to_string(),
        artifact_bytes: 96_000,
        artifact_sha256: sha256(b"canonical artifact"),
        original_output_sha256: sha256(b"raw output before bounding"),
        original_tokens: 24_000,
        preserved_non_text_tokens: Some(0),
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

fn named_function_call_with_arguments(
    call_id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: name.to_string(),
        namespace: None,
        arguments: arguments.to_string(),
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
        unavailable: false,
        repository_root: Some(format!("/repo-{label}")),
        head_identity: Some(format!("head-{label}")),
        index_identity: Some(format!("index-{label}")),
        worktree_identity: Some(format!("worktree-{label}")),
        path_fingerprints: None,
    }
}

#[test]
fn workspace_identity_serialization_preserves_failure_and_reads_legacy_identity() {
    let legacy = serde_json::json!({
        "repository_root": "/repo",
        "head_identity": "head",
        "index_identity": "index",
        "worktree_identity": "worktree",
    });
    let mut identity: WorkspaceEvidenceIdentity = serde_json::from_value(legacy.clone()).unwrap();
    assert!(!identity.unavailable);
    assert_eq!(serde_json::to_value(&identity).unwrap(), legacy);
    identity.unavailable = true;
    let serialized = serde_json::to_value(&identity).unwrap();
    assert_eq!(serialized["unavailable"], true);
    let restored: WorkspaceEvidenceIdentity = serde_json::from_value(serialized).unwrap();
    assert!(restored.unavailable);
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
            BTreeSet::from([SourceDependencyV1::new(Path::new("/repo-captured"), true)]),
        )
        .expect("text evidence observation"),
    );

    let current = state.project_with_workspace_identity(Arc::clone(&canonical), Some(&captured));
    assert_eq!(current.items, canonical);

    let stale = state.project_with_workspace_identity(canonical, Some(&changed));
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("\"stale_workspace_evidence\":true"));
    assert_eq!(stale.unreplaced_items, stale.items);
    let notice: serde_json::Value = serde_json::from_str(stale_output).unwrap();
    assert_eq!(notice["reason_code"], "workspace_identity_changed");
    assert_eq!(notice["valid_for_current_workspace"], false);
    let instruction = notice["rerun"]["instruction"].as_str().unwrap();
    assert!(instruction.contains("This notice is not a request to rerun tests or builds."));
    assert!(instruction.contains("Revalidate only if current evidence is essential to the task"));
    assert!(instruction.contains("otherwise report the affected claim as unverified"));
    assert_eq!(
        notice["observed_revision"],
        serde_json::to_value(&captured).unwrap()
    );
    assert_eq!(
        notice["current_revision"],
        serde_json::to_value(&changed).unwrap()
    );
    assert!(notice["rerun"].get("force_fresh").is_none());
    assert!(
        notice["rerun"]["instruction"]
            .as_str()
            .unwrap()
            .contains("supported arguments")
    );
    assert_eq!(
        notice["if_rerun_unavailable"],
        "Report the affected claim as unverified; this result does not validate the current workspace."
    );
}

#[test]
fn workspace_projection_memoizes_verdicts_without_crossing_mutations_or_snapshots() {
    let captured = workspace_identity("captured");
    let changed = workspace_identity("changed");
    let output = text_output("cached", "captured evidence".into());
    let items: Arc<[ResponseItem]> = Arc::from([function_call("cached"), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(WorkspaceEvidenceObservation::from_response_item(
        Some(captured.clone()), &output,
        BTreeSet::from([SourceDependencyV1::new(Path::new("/repo-captured"), true)]),
    ).unwrap());
    let original = state.clone();
    let project = |state: &ToolHistoryState, identity: &WorkspaceEvidenceIdentity| {
        state.project_with_workspace_identity(Arc::clone(&items), Some(identity)).items
    };
    let hits = |state: &ToolHistoryState| {
        state.workspace_projection_cache.lock().unwrap()["cached"].hits
    };
    assert_eq!(project(&state, &captured), items);
    assert_eq!(project(&state, &captured), items);
    assert_eq!(hits(&state), 1);
    let stale = project(&state, &changed);
    assert_ne!(stale, items);
    assert_eq!(hits(&state), 0);
    assert_eq!(project(&state.clone(), &changed), stale);
    assert_eq!(hits(&state), 1);
    assert!(state.invalidate_source_dependencies(None, Some(&captured)));
    assert_ne!(project(&state, &captured), items);
    assert_eq!(hits(&state), 0);
    assert_eq!(project(&original, &captured), items);
    assert_eq!(hits(&state), 0);
    let encoded = serde_json::to_value(&state).unwrap();
    assert!(encoded.get("workspace_projection_cache").is_none());
    let restored: ToolHistoryState = serde_json::from_value(encoded).unwrap();
    assert!(restored.workspace_projection_cache.lock().unwrap().is_empty());
    assert_ne!(project(&restored, &captured), items);
    state.project_with_workspace_identity(Arc::from([]), Some(&captured));
    assert!(state.workspace_projection_cache.lock().unwrap().is_empty());
}

#[test]
fn workspace_projection_cache_rechecks_output_and_read_arguments() {
    let identity = workspace_identity("captured");
    let output = text_output("read", "old evidence".into());
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(WorkspaceEvidenceObservation::from_response_item(
        Some(identity.clone()), &output, BTreeSet::new(),
    ).unwrap());
    state.invalidate_source_dependencies(None, Some(&identity));
    let mut call = function_call("read");
    if let ResponseItem::FunctionCall { name, arguments, .. } = &mut call {
        *name = "read_file".into();
        *arguments = r#"{"path":"before.rs"}"#.into();
    }
    let first = state.project_with_workspace_identity(
        Arc::from([call.clone(), output.clone()]), Some(&identity),
    );
    if let ResponseItem::FunctionCall { arguments, .. } = &mut call {
        *arguments = r#"{"path":"after.rs"}"#.into();
    }
    let second = state.project_with_workspace_identity(
        Arc::from([call.clone(), output]), Some(&identity),
    );
    assert_ne!(first.items[1], second.items[1]);
    let tampered = state.project_with_workspace_identity(
        Arc::from([call, text_output("read", "unverified replacement".into())]), Some(&identity),
    );
    let (_, text) = textual_output_identity(&tampered.items[1]).unwrap();
    assert!(!text.contains("historical_digest"));
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
            BTreeSet::from([SourceDependencyV1::new(Path::new("/repo-after"), true)]),
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
fn workspace_freshness_notice_distinguishes_unknown_from_changed_identity() {
    let call_id = "freshness-reasons";
    let captured = workspace_identity("captured");
    let changed = workspace_identity("changed");
    let unavailable = WorkspaceEvidenceIdentity {
        unavailable: true,
        ..captured.clone()
    };
    let output = text_output(call_id, "source evidence".to_string());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    for (observed, current, expected) in [
        (
            Some(captured.clone()),
            None,
            "workspace_identity_unavailable",
        ),
        (
            Some(unavailable.clone()),
            Some(&captured),
            "workspace_identity_unavailable",
        ),
        (
            Some(captured.clone()),
            Some(&unavailable),
            "workspace_identity_unavailable",
        ),
        (
            Some(captured.clone()),
            Some(&changed),
            "workspace_identity_changed",
        ),
    ] {
        let mut state = ToolHistoryState::default();
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(observed, &output, BTreeSet::new())
                .unwrap(),
        );
        let projected = state.project_with_workspace_identity(Arc::clone(&canonical), current);
        let (_, text) = textual_output_identity(&projected.items[1]).unwrap();
        let notice: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(notice["reason_code"], expected);
        assert_eq!(notice["valid_for_current_workspace"], false);
        assert!(notice["rerun"].get("force_fresh").is_none());
    }
}

#[test]
fn non_git_workspace_evidence_is_historical_after_external_edit() {
    let call_id = "non-git-call";
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("source.txt");
    std::fs::write(&path, "plain directory output").unwrap();
    let output = text_output(call_id, std::fs::read_to_string(&path).unwrap());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call(call_id), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(None, &output, BTreeSet::new())
            .expect("non-git observation"),
    );

    std::fs::write(&path, "externally changed").unwrap();
    let current = state.project_with_workspace_identity(Arc::clone(&canonical), None);
    let (_, text) = textual_output_identity(&current.items[1]).unwrap();
    let notice: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(notice["reason_code"], "workspace_identity_unavailable");
    assert_eq!(notice["historical_digest"], "plain directory output");
    assert_eq!(notice["valid_for_current_workspace"], false);

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
    assert_ne!(current.items, canonical, "unknown freshness is not current proof");

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
    assert!(output.contains("no workspace observation is available"));
    assert!(output.contains("unrecorded or evicted"));
    let notice: serde_json::Value = serde_json::from_str(output).unwrap();
    assert_eq!(notice["reason_code"], "missing_observation");
    assert_eq!(notice["valid_for_current_workspace"], false);
    assert_eq!(notice["observed_revision"], serde_json::Value::Null);
    assert!(!output.contains("repository identity"));
}

#[test]
fn workspace_evidence_output_mismatch_does_not_claim_the_repository_changed() {
    let call_id = "changed-output";
    let captured = workspace_identity("captured");
    let output = text_output(call_id, "original file contents".to_string());
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured.clone()),
            &output,
            BTreeSet::new(),
        )
        .expect("text evidence observation"),
    );
    let canonical = Arc::from([
        function_call(call_id),
        text_output(call_id, "different output".to_string()),
    ]);
    let projection = state.project_with_workspace_identity(canonical, Some(&captured));
    let (_, output) = textual_output_identity(&projection.items[1]).expect("stale output");
    let notice: serde_json::Value = serde_json::from_str(output).expect("freshness notice");
    assert_eq!(notice["stale_workspace_evidence"], true);
    assert_eq!(notice["reason_code"], "output_mismatch");
    assert!(notice["rerun"].get("force_fresh").is_none());
    assert_eq!(
        notice["reason"],
        "the tool output does not match its recorded workspace observation; it is not verified evidence"
    );
    assert!(!output.contains("different output"));
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
    let notice: serde_json::Value = serde_json::from_str(output).unwrap();
    assert_eq!(notice["historical_output"], "old file contents");
    assert_eq!(notice["valid_for_current_workspace"], false);
    assert!(
        !state.workspace_evidence[origin_call_id]
            .is_current(Some(&workspace_identity("changed")), None)
    );
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

    let after_overlap = workspace_identity("after-overlap");
    assert!(
        state.invalidate_source_dependencies(Some(&BTreeSet::from([foo])), Some(&after_overlap),)
    );
    let stale = state.project_with_workspace_identity(canonical, Some(&after_overlap));
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("source dependencies were invalidated after capture"));
}

#[test]
fn recovered_bytes_survive_missing_producer_provenance() {
    let output = "exact historical source recovered successfully";
    let canonical = Arc::from([
        named_function_call_with_arguments(
            "recovery",
            "read_tool_output",
            serde_json::json!({"artifact_id":"unknown-producer"}),
        ),
        text_output("recovery", output.to_string()),
    ]);
    let projection = ToolHistoryState::default()
        .project_with_workspace_identity(canonical, Some(&workspace_identity("current")));
    let (_, text) = textual_output_identity(&projection.items[1]).unwrap();
    let notice: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(notice["reason_code"], "missing_observation");
    assert_eq!(notice["historical_output"], output);
    assert_eq!(notice["valid_for_current_workspace"], false);
    assert!(
        notice["rerun"]["instruction"]
            .as_str()
            .unwrap()
            .contains("rereading this immutable artifact cannot establish freshness")
    );
}

#[test]
fn investigation_evidence_stays_raw_across_consumed_generations() {
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    // A closeout needs several substantial source reads and the actual validation
    // result. Merely seeing them once must not replace them with recovery receipts.
    for index in 0..14 {
        let call_id = format!("evidence-{index}");
        let output = format!("source-{index}\n{}", "detail ".repeat(2_600));
        let mut record = candidate(&call_id, output.clone());
        record.artifact_id = format!("00000000-0000-7000-8000-{index:012}");
        record.consumed_by_generation = Some(ModelGenerationId {
            turn_id: "investigation".into(),
            ordinal: 1,
        });
        state.register(record);
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, output));
    }
    let original = Arc::<[ResponseItem]>::from(items);
    let tokens = original
        .iter()
        .filter_map(textual_output_identity)
        .map(|(_, text)| approx_token_count(text))
        .sum::<usize>();
    assert!(
        tokens > 60_000 && tokens <= 75_000,
        "fixture must exercise the increased budget: {tokens}"
    );
    let projection = state.project(Arc::clone(&original));
    assert_eq!(projection.items.as_ref(), original.as_ref());
}

#[test]
fn small_budget_excess_compacts_only_oldest_consumed_results() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(60_000);
    let output = "x ".repeat(1_000);
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for index in 0..61 {
        let call_id = format!("call-{index}");
        let mut record = candidate(&call_id, output.clone());
        record.artifact_id = format!("00000000-0000-7000-8000-{index:012}");
        if index < 60 {
            record.consumed_by_generation = Some(ModelGenerationId {
                turn_id: "turn".into(),
                ordinal: 1,
            });
        }
        state.register(record);
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, output.clone()));
    }
    let projection = state.project(Arc::from(items));
    let outputs = projection
        .items
        .iter()
        .filter_map(textual_output_identity)
        .collect::<Vec<_>>();
    assert_eq!(outputs.len(), 61, "all outputs must remain recoverable");
    let raw_count = outputs.iter().filter(|(_, text)| *text == output).count();
    assert!(
        raw_count >= 58,
        "small excess must not compact all consumed results: {raw_count}"
    );
    assert_eq!(outputs.last().unwrap().1, output);
    assert!(
        outputs
            .iter()
            .map(|(_, text)| approx_token_count(text))
            .sum::<usize>()
            <= 60_000
    );
}

#[test]
fn workspace_error_outputs_preserve_diagnostics_with_historical_applicability() {
    let call_id = "failed-workspace-read";
    let dependency = PathBuf::from("/repo/src/foo.rs");
    let mut payload = FunctionCallOutputPayload::from_text("permission denied".to_string());
    payload.success = Some(false);
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: payload,
        internal_chat_message_metadata_passthrough: None,
    };
    let canonical: Arc<[ResponseItem]> = Arc::from([
        named_function_call_with_arguments(
            call_id,
            "exec_command",
            serde_json::json!({
                "kind": "argv",
                "program": "rg",
                "args": ["needle", "src/foo.rs"]
            }),
        ),
        output.clone(),
    ]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(workspace_identity("captured")),
            &output,
            BTreeSet::from([SourceDependencyV1::new(&dependency, false)]),
        )
        .expect("failed workspace observation"),
    );

    assert!(state.invalidate_source_dependencies(
        Some(&BTreeSet::from([dependency])),
        Some(&workspace_identity("changed")),
    ));
    let projection = state.project_with_workspace_identity(
        Arc::clone(&canonical),
        Some(&workspace_identity("changed")),
    );
    let (_, text) = textual_output_identity(&projection.items[1]).unwrap();
    let notice: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(notice["historical_output"], "permission denied");
    assert_eq!(notice["valid_for_current_workspace"], false);
    assert!(notice["failure_applicability"].as_str().unwrap().contains("current applicability is unverified"));
    assert_eq!(response_item_output_success(&projection.items[1]), Some(false));
}

#[tokio::test]
async fn intersecting_workspace_transition_marks_stale_and_permits_suppressed_rerun() {
    let call_id = "stale-search";
    let dependency = PathBuf::from("/repo/src/foo.rs");
    let arguments = serde_json::json!({
        "program": "rg",
        "args": ["needle", "src/foo.rs"]
    });
    let mut payload = FunctionCallOutputPayload::from_text("src/foo.rs:needle".to_string());
    payload.success = Some(true);
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: call_id.to_string(),
        output: payload,
        internal_chat_message_metadata_passthrough: None,
    };
    let canonical: Arc<[ResponseItem]> = Arc::from([
        named_function_call_with_arguments(call_id, "exec_command", arguments),
        output.clone(),
    ]);
    let captured = workspace_identity("captured");
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(captured),
            &output,
            BTreeSet::from([SourceDependencyV1::new(&dependency, false)]),
        )
        .expect("successful workspace observation"),
    );

    let search = RgSearchNarrowing {
        breadth: RgSearchBreadth::Narrow,
        query_identity: "needle".to_string(),
        search_identity: "needle:src/foo.rs".to_string(),
        scope_identity: "src/foo.rs".to_string(),
        parent_scope_identity: Some("repo".to_string()),
        scope_state_identity: Some("scope-state".to_string()),
        state_paths: Vec::new(),
        can_record_miss: true,
    };
    let command = [
        "rg".to_string(),
        "needle".to_string(),
        "src/foo.rs".to_string(),
    ];
    let attempt_key = CommandAttemptKey::new("exec_command", "local", "/repo", &command)
        .with_repository_epoch(1)
        .with_workspace_identity(Some("workspace-a"))
        .with_search_narrowing("turn-a", "repo-a", Some(search.clone()));
    let ledger = CommandExecutionLedger::default();
    ledger
        .begin_attempt_with_freshness(&attempt_key, false, false)
        .await
        .expect("initial search runs");
    ledger.record_exit(&attempt_key, 1).await;
    ledger
        .begin_attempt_with_freshness(&attempt_key, false, false)
        .await
        .expect_err("unchanged negative search is suppressed");

    let after_unrelated = workspace_identity("after-unrelated");
    assert!(state.invalidate_source_dependencies(
        Some(&BTreeSet::from([PathBuf::from("/repo/src/bar.rs")])),
        Some(&after_unrelated),
    ));
    let unrelated =
        state.project_with_workspace_identity(Arc::clone(&canonical), Some(&after_unrelated));
    assert_eq!(unrelated.items, canonical);

    let after_overlap = workspace_identity("after-overlap");
    assert!(
        state.invalidate_source_dependencies(
            Some(&BTreeSet::from([dependency])),
            Some(&after_overlap),
        )
    );
    let stale = state.project_with_workspace_identity(Arc::clone(&canonical), Some(&after_overlap));
    assert_eq!(
        stale.items[0], canonical[0],
        "preserve the historical invocation"
    );
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    let notice: serde_json::Value = serde_json::from_str(stale_output).expect("stale notice");
    assert!(notice["rerun"].get("force_fresh").is_none());
    assert!(
        notice["rerun"]["instruction"]
            .as_str()
            .unwrap()
            .contains("supported arguments")
    );
    let refreshed_key = attempt_key.with_repository_epoch(2).with_search_narrowing(
        "turn-a",
        "repo-a",
        Some(RgSearchNarrowing {
            scope_state_identity: Some("changed-scope-state".to_string()),
            ..search
        }),
    );
    ledger
        .begin_attempt_with_freshness(&refreshed_key, false, false)
        .await
        .expect("a changed workspace runs the supported arguments without a freshness override");
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

#[tokio::test]
async fn watcher_proof_retains_dependency_scoped_evidence_after_external_disjoint_edit() {
    let root = tempfile::tempdir().expect("workspace root");
    let source = root.path().join("src/foo.rs");
    std::fs::create_dir_all(source.parent().expect("source parent")).expect("create source dir");
    std::fs::write(&source, "fn foo() {}\n").expect("write source");
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let path_observation = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .await
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

    cache
        .note_host_workspace_mutation_paths(root.path(), &["README.md".to_string()])
        .await;
    let unrelated =
        state.project_with_workspace_cache(Arc::clone(&canonical), Some(&changed), cache.as_ref());
    assert_eq!(unrelated.items, canonical);

    cache
        .note_host_workspace_mutation_paths(root.path(), &["src/foo.rs".to_string()])
        .await;
    let stale = state.project_with_workspace_cache(Arc::clone(&canonical), Some(&changed), cache.as_ref());
    let (_, stale_output) = textual_output_identity(&stale.items[1]).expect("stale output");
    assert!(stale_output.contains("stale_workspace_evidence"));
    let notice: serde_json::Value = serde_json::from_str(&stale_output).unwrap();
    assert_eq!(notice["workspace_evidence_freshness"], "changed");
    assert_eq!(notice["qualification"], "Observed dependency change");
    assert_eq!(notice["historical_authenticity"], "authenticated");
    cache.note_host_workspace_mutation();
    let unknown = state.project_with_workspace_cache(canonical, Some(&changed), cache.as_ref());
    let (_, output) = textual_output_identity(&unknown.items[1]).unwrap();
    let notice: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(notice["workspace_evidence_freshness"], "unknown");
    assert_eq!(notice["qualification"], "Currentness unknown; no dependency change established");
    assert_eq!(notice["historical_authenticity"], "authenticated");
    assert_eq!(notice["valid_for_current_workspace"], false);
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

#[tokio::test]
async fn external_source_evidence_survives_repo_edits_but_not_attachment_changes() {
    let root = tempfile::tempdir().unwrap();
    let attachments = tempfile::tempdir().unwrap();
    let source = root.path().join("source.rs");
    let attachment = attachments.path().join("prompt.txt");
    std::fs::write(&source, "source").unwrap();
    std::fs::write(&attachment, "original requirements").unwrap();
    let cache = GitWorkspaceCache::new();
    let observations = cache
        .begin_source_path_change_observations(
            root.path(),
            &[(source.clone(), false), (attachment.clone(), false)],
        )
        .await
        .expect("native watches for repository and attachment");
    assert_eq!(observations.len(), 2, "no dependency may be silently dropped");
    let mut before = workspace_identity("before");
    before.repository_root = Some(root.path().to_string_lossy().into_owned());
    let mut after = workspace_identity("unrelated-repo-edit");
    after.repository_root = before.repository_root.clone();
    let output = text_output("mixed-read", "source and original requirements".into());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call("mixed-read"), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(before),
            &output,
            BTreeSet::from([
                SourceDependencyV1::new(&source, false),
                SourceDependencyV1::new(&attachment, false),
            ]),
        )
        .unwrap()
        .with_source_path_observations(observations.clone()),
    );
    std::fs::write(root.path().join("unrelated.txt"), "unrelated edit").unwrap();
    cache
        .note_host_workspace_mutation_paths(root.path(), &["unrelated.txt".into()])
        .await;
    assert_eq!(
        state
            .project_with_workspace_cache(Arc::clone(&canonical), Some(&after), &cache)
            .items,
        canonical,
        "an unrelated repository edit must preserve both unchanged inputs"
    );

    std::fs::write(&attachment, "changed requirements").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while cache.source_path_change_observation_is_current(&observations[1]) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("native watcher must observe the external attachment changing");
    assert!(cache.source_path_change_observation_is_current(&observations[0]));
    let projected = state.project_with_workspace_cache(canonical, Some(&after), &cache);
    let (_, output) = textual_output_identity(&projected.items[1]).unwrap();
    let notice: serde_json::Value = serde_json::from_str(output).unwrap();
    assert_eq!(notice["stale_workspace_evidence"], true);
    assert_eq!(notice["valid_for_current_workspace"], false);
}

#[test]
fn nested_workspace_evidence_retains_only_current_results_after_resume() {
    let mut state = ToolHistoryState::default();
    let before = workspace_identity("before");
    let after = workspace_identity("after");
    let parent = text_output("parent", "combined A and B".into());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call("parent"), parent.clone()]);
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(before.clone()),
            &parent,
            BTreeSet::new(),
        )
        .expect("parent"),
    );
    for (id, path, output) in [("a", "/repo/a", "old A"), ("b", "/repo/b", "current B")] {
        let nested = text_output(id, output.into());
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(
                Some(before.clone()),
                &nested,
                BTreeSet::from([SourceDependencyV1::new(Path::new(path), false)]),
            )
            .expect("nested"),
        );
        assert!(
            ToolHistoryMutation::RegisterCodeModeNestedEvidence {
                parent_call_id: "parent".into(),
                call_id: id.into(),
                output: output.into(),
            }
            .apply(&mut state)
        );
    }
    state.retain_for_history(&canonical);
    // The nested calls are not standalone history items. Their evidence must
    // survive pruning and durable serialization with the live parent.
    let encoded = serde_json::to_vec(&state).expect("serialize ledger");
    let mut state: ToolHistoryState = serde_json::from_slice(&encoded).expect("resume ledger");
    state.invalidate_source_dependencies(
        Some(&BTreeSet::from([PathBuf::from("/repo/a")])),
        Some(&after),
    );
    let projected = state.project_with_workspace_identity(Arc::clone(&canonical), Some(&after));
    let (_, output) = textual_output_identity(&projected.items[1]).expect("parent result");
    let notice: serde_json::Value = serde_json::from_str(output).expect("freshness notice");
    let current = notice["current_nested_results"].as_array().unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["call_id"], "b");
    assert_eq!(current[0]["output"], "current B");
    assert_eq!(current[0]["workspace_evidence_freshness"], "current");
    assert_eq!(current[0]["source_scope"]["dependencies"][0]["path"],
        SourceDependencyV1::new(Path::new("/repo/b"), false).path);
    assert_eq!(notice["stale_nested_results"][0]["call_id"], "a");
    assert_eq!(notice["stale_nested_results"][0]["source_scope"]["dependencies"][0]["path"],
        SourceDependencyV1::new(Path::new("/repo/a"), false).path);
    assert!(!output.contains("old A"));
    assert_eq!(notice["historical_digest"], "combined A and B");
    assert_eq!(notice["valid_for_current_workspace"], false);

    let mut mutated = state.clone();
    mutated.invalidate_source_dependencies(
        Some(&BTreeSet::from([PathBuf::from("/repo/b")])),
        Some(&after),
    );
    let projected = mutated.project_with_workspace_identity(Arc::clone(&canonical), Some(&after));
    let (_, output) = textual_output_identity(&projected.items[1]).expect("mutated parent");
    assert!(!output.contains("current B"));
    let projected = state.project_with_workspace_identity(Arc::clone(&canonical), Some(&after));
    let (_, output) = textual_output_identity(&projected.items[1]).expect("original snapshot");
    assert!(output.contains("current B"));

    // An unobserved external revision has no proof that B stayed unchanged.
    let projected =
        state.project_with_workspace_identity(canonical, Some(&workspace_identity("external")));
    let (_, output) = textual_output_identity(&projected.items[1]).expect("stale parent");
    assert!(!output.contains("current B"));
    state.retain_for_history(&[]);
    assert!(state.is_persisted_empty());
}

#[test]
fn command_dependencies_cover_search_test_and_read_inputs() {
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
        BTreeSet::new(),
        "unscoped Cargo commands cannot establish a selective dependency graph"
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

    let powershell = ToolPayload::Function {
        arguments: serde_json::json!({
            "kind": "powershell_script",
            "script_body": "rg needle src/foo.rs"
        })
        .to_string(),
    };
    assert_eq!(
        source_dependencies_for_tool_call("exec_command", &powershell, cwd),
        BTreeSet::from([SourceDependencyV1::new(
            Path::new("/repo/src/foo.rs"),
            false,
        )])
    );
}

#[test]
fn quoted_and_batched_file_reads_keep_all_source_dependencies() {
    let cwd = Path::new("/repo");
    let mut commands = vec![("bash", "cat 'contract one.txt'; cat 'contract two.txt'")];
    if cfg!(windows) {
        commands.push((
            "powershell",
            "Get-Content -LiteralPath 'contract one.txt'; Get-Content -LiteralPath 'contract two.txt'",
        ));
    }
    for (shell, command) in commands {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"cmd": command, "shell": shell}).to_string(),
        };
        let classification = classify_workspace_tool_call("exec_command", &payload, cwd);
        assert!(classification.observes_workspace);
        assert_eq!(
            classification.source_dependencies,
            BTreeSet::from([
                SourceDependencyV1::new(&cwd.join("contract one.txt"), false),
                SourceDependencyV1::new(&cwd.join("contract two.txt"), false),
            ]),
            "{shell}: {command}",
        );
    }
}

#[test]
fn file_read_dependencies_require_complete_literal_bounded_scope() {
    let cwd = Path::new("/repo");
    for (shell, command) in [
        ("bash", "cat 'contract.txt'; python check.py"),
        ("bash", "cat 'contract.txt'; cat \"$OTHER_FILE\""),
        ("bash", "cat 'contract.txt'; cat $(printf other.txt)"),
        ("bash", "cat 'contract.txt'; cat *.txt"),
        ("bash", "cat 'contract.txt'; cat ~/other.txt"),
        (
            "powershell",
            "Get-Content 'contract.txt'; Get-Content '~/other.txt'",
        ),
        ("bash", r"cat 'contract.txt'; cat other\ file.txt"),
        ("bash", "cd subdir; cat 'contract.txt'"),
        ("cmd", r#"type "contract.txt" & type "%OTHER_FILE%""#),
        ("cmd", r#"type "contract.txt" & type "!OTHER_FILE!""#),
    ] {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"cmd": command, "shell": shell}).to_string(),
        };
        let classification = classify_workspace_tool_call("exec_command", &payload, cwd);
        assert!(classification.observes_workspace);
        assert!(
            classification.source_dependencies.is_empty(),
            "{shell}: {command}"
        );
    }
}

#[test]
fn nine_literal_paths_retain_precise_dependencies() {
    let cwd = Path::new("/repo");
    let names = (0..9)
        .map(|index| format!("file-{index}.rs"))
        .collect::<Vec<_>>();
    let expected = names
        .iter()
        .map(|name| SourceDependencyV1::new(&cwd.join(name), false))
        .collect::<BTreeSet<_>>();
    for command in [
        format!("cat {}", names.join(" ")),
        names
            .iter()
            .map(|name| format!("cat {name}"))
            .collect::<Vec<_>>()
            .join("; "),
        format!("rg needle {}", names.join(" ")),
    ] {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"cmd":command,"shell":"bash"}).to_string(),
        };
        assert_eq!(
            classify_workspace_tool_call("exec_command", &payload, cwd).source_dependencies,
            expected,
            "{command}"
        );
    }
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

/// Regression for the recorded sessions where `Get-Content x | Select-Object
/// ...` batches carried no dependencies, so every workspace change marked them
/// stale and the model re-read unchanged files dozens of times.
#[test]
fn pipeline_stages_and_host_queries_keep_batched_read_dependencies() {
    let cwd = Path::new("/repo");
    let expected = BTreeSet::from([
        SourceDependencyV1::new(&cwd.join("src/effects.rs"), false),
        SourceDependencyV1::new(&cwd.join("src/work.rs"), false),
    ]);
    let mut commands = vec![
        (
            "bash",
            "cat src/effects.rs | head -n 40; cat src/work.rs | tail -n 20 | wc -l; echo done",
        ),
        (
            "bash",
            "rg -n 'fn run' src/effects.rs; cat src/work.rs | sort | uniq",
        ),
    ];
    if cfg!(windows) {
        commands.push((
            "powershell",
            "Get-Content src/effects.rs | Select-Object -Skip 1380 -First 190; Get-Content src/work.rs | Select-Object -Last 22; Get-CimInstance Win32_Process | Select-Object ProcessId",
        ));
        commands.push((
            "powershell",
            "rg -n 'fn run' src/effects.rs | Select-Object -First 65; Get-Content src/work.rs -TotalCount 320; Write-Output ready",
        ));
    }
    for (shell, command) in commands {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"cmd": command, "shell": shell}).to_string(),
        };
        let classification = classify_workspace_tool_call("exec_command", &payload, cwd);
        assert!(classification.observes_workspace);
        assert_eq!(
            classification.source_dependencies, expected,
            "{shell}: {command}"
        );
    }
}

#[test]
fn search_and_read_in_one_batch_keep_both_scopes() {
    // The previous batch rule kept only the search scope, so an edit to the
    // read file could not invalidate the batch that displayed it.
    let cwd = Path::new("/repo");
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "cmd": "rg -n 'fn function' src/analysis_context.rs; cat src/work.rs",
            "shell": "bash"
        })
        .to_string(),
    };
    assert_eq!(
        classify_workspace_tool_call("exec_command", &payload, cwd).source_dependencies,
        BTreeSet::from([
            SourceDependencyV1::new(&cwd.join("src/analysis_context.rs"), false),
            SourceDependencyV1::new(&cwd.join("src/work.rs"), false),
        ])
    );
}

#[test]
fn listings_git_reads_and_path_cmdlets_scope_without_making_batches_opaque() {
    let cwd = Path::new("/repo");
    let work = SourceDependencyV1::new(&cwd.join("src/work.rs"), false);
    let checkout = SourceDependencyV1::new(cwd, true);
    let src_tree = SourceDependencyV1::new(&cwd.join("src"), true);
    let mut cases = vec![
        (
            "bash",
            "git status --short; cat src/work.rs",
            BTreeSet::from([checkout.clone(), work.clone()]),
        ),
        (
            "bash",
            "ls -la src; cat src/work.rs",
            BTreeSet::from([src_tree.clone(), work.clone()]),
        ),
        ("bash", "ls", BTreeSet::from([checkout.clone()])),
    ];
    if cfg!(windows) {
        cases.push((
            "powershell",
            "Get-ChildItem src -Filter AGENTS.md; Get-Content src/work.rs",
            BTreeSet::from([src_tree, work.clone()]),
        ));
        cases.push((
            "powershell",
            "Get-Item src/work.rs; Test-Path src/effects.rs",
            BTreeSet::from([
                work.clone(),
                SourceDependencyV1::new(&cwd.join("src/effects.rs"), false),
            ]),
        ));
        cases.push((
            "powershell",
            "Select-String -Path work.log -Pattern 'FAILED'; Get-Content work.log -Tail 5",
            BTreeSet::from([SourceDependencyV1::new(&cwd.join("work.log"), false)]),
        ));
        cases.push((
            "powershell",
            "Get-Content work.log | Select-String 'FAILED'",
            BTreeSet::from([SourceDependencyV1::new(&cwd.join("work.log"), false)]),
        ));
        cases.push((
            "powershell",
            "git diff --stat; Get-Content src/work.rs",
            BTreeSet::from([checkout, work]),
        ));
    }
    for (shell, command, expected) in cases {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"cmd": command, "shell": shell}).to_string(),
        };
        assert_eq!(
            classify_workspace_tool_call("exec_command", &payload, cwd).source_dependencies,
            expected,
            "{shell}: {command}"
        );
    }
}

#[test]
fn select_string_named_pattern_retains_positional_paths() {
    let cases: &[&[&str]] = &[
        &["-Pattern", "needle", "src/second.rs"],
        &["src/second.rs", "-Pattern", "needle"],
        &["src/second.rs", "-pAtTeRn", "needle"],
        &["needle", "src/second.rs"],
        &["-Pattern", "needle", "-Path", "src/second.rs"],
        &["-Path", "src/second.rs", "needle"],
    ];
    for case in cases {
        let arguments = case.iter().map(|word| (*word).to_string()).collect::<Vec<_>>();
        assert_eq!(
            powershell_path_operands(&arguments, &["-pattern"], true),
            Some(vec!["src/second.rs".to_string()]),
            "{case:?}"
        );
    }
    assert_eq!(
        powershell_path_operands(&["-Pattern".into(), "needle".into()], &["-pattern"], true),
        Some(Vec::new()),
        "a named pattern without a path remains a pipeline filter"
    );
    for case in [&["-Pattern"][..], &["-Pattern", "-Path", "src/second.rs"][..]] {
        let arguments = case.iter().map(|word| (*word).to_string()).collect::<Vec<_>>();
        assert_eq!(powershell_path_operands(&arguments, &["-pattern"], true), None);
    }
    let wildcard = powershell_path_operands(
        &["-Pattern".into(), "needle".into(), "src/*.rs".into()],
        &["-pattern"],
        true,
    );
    assert!(matches!(
        literal_path_dependencies(wildcard, Path::new("/repo")),
        Some(PlainCommandDependencies::Unknown)
    ));
}

#[cfg(windows)]
#[test]
fn select_string_named_pattern_batch_invalidates_when_positional_path_changes() {
    let cwd = Path::new("/repo");
    for search in [
        "Select-String -Pattern 'needle' src/second.rs",
        "Select-String src/second.rs -pAtTeRn 'needle'",
    ] {
        let arguments = serde_json::json!({
            "cmd": format!("Get-Content src/first.rs; {search}"),
            "shell": "powershell",
        });
        let payload = ToolPayload::Function { arguments: arguments.to_string() };
        let classification = classify_workspace_tool_call("exec_command", &payload, cwd);
        assert_eq!(
            classification.source_dependencies,
            BTreeSet::from([
                SourceDependencyV1::new(&cwd.join("src/first.rs"), false),
                SourceDependencyV1::new(&cwd.join("src/second.rs"), false),
            ]),
            "{search}"
        );
        let call_id = "named-pattern-batch";
        let output = text_output(call_id, "first file and matching second file".to_string());
        let canonical: Arc<[ResponseItem]> = Arc::from([
            named_function_call_with_arguments(call_id, "exec_command", arguments),
            output.clone(),
        ]);
        let mut state = ToolHistoryState::default();
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item(
                Some(workspace_identity("captured")),
                &output,
                classification.source_dependencies,
            )
            .expect("batch observation"),
        );
        let unrelated = workspace_identity("unrelated");
        state.invalidate_source_dependencies(
            Some(&BTreeSet::from([cwd.join("notes.txt")])),
            Some(&unrelated),
        );
        assert_eq!(
            state.project_with_workspace_identity(Arc::clone(&canonical), Some(&unrelated)).items,
            canonical
        );
        let changed = workspace_identity("second-file-changed");
        state.invalidate_source_dependencies(
            Some(&BTreeSet::from([cwd.join("src/second.rs")])),
            Some(&changed),
        );
        let projected = state.project_with_workspace_identity(canonical, Some(&changed));
        let (_, text) = textual_output_identity(&projected.items[1]).expect("stale result");
        let notice: serde_json::Value = serde_json::from_str(text).expect("stale evidence notice");
        assert_eq!(notice["stale_workspace_evidence"], true, "{search}");
        assert_eq!(notice["reason_code"], "source_dependencies_invalidated", "{search}");
    }
}

#[test]
fn commands_that_may_read_anywhere_still_make_the_batch_opaque() {
    let cwd = Path::new("/repo");
    let mut commands = vec![
        ("bash", "cat src/work.rs | xargs cat"),
        ("bash", "cat src/work.rs; sort other.txt"),
        ("bash", "cat src/work.rs; git checkout -- src/work.rs"),
        ("bash", "cat src/work.rs; cargo check"),
    ];
    if cfg!(windows) {
        commands.push(("powershell", "Get-Content src/work.rs; cargo check --tests"));
    }
    for (shell, command) in commands {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"cmd": command, "shell": shell}).to_string(),
        };
        assert!(
            classify_workspace_tool_call("exec_command", &payload, cwd)
                .source_dependencies
                .is_empty(),
            "{shell}: {command}"
        );
    }
}

#[test]
fn tool_result_budget_caps_large_model_context_windows() {
    assert_eq!(model_visible_tool_result_token_budget_for_context_window(Some(760_000)), 75_000);
    assert_eq!(model_visible_tool_result_token_budget_for_context_window(Some(32_000)), 16_000);
    assert_eq!(
        model_visible_tool_result_token_budget_for_context_window(None),
        model_visible_tool_result_token_budget()
    );
    assert_eq!(
        model_visible_tool_result_token_budget_for_context_window(Some(0)),
        model_visible_tool_result_token_budget()
    );
    assert_eq!(
        model_visible_tool_result_token_budget_for_context_window(Some(258_400)),
        75_000
    );

    // A configured window budget replaces the fixed default for projection.
    let output = "x ".repeat(1_000);
    let mut items = Vec::new();
    let mut state = ToolHistoryState::default();
    for index in 0..8 {
        let call_id = format!("call-{index}");
        let mut record = candidate(&call_id, output.clone());
        record.artifact_id = format!("00000000-0000-7000-8000-{index:012}");
        record.consumed_by_generation = Some(ModelGenerationId {
            turn_id: "turn".into(),
            ordinal: 1,
        });
        state.register(record);
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, output.clone()));
    }
    let items = Arc::<[ResponseItem]>::from(items);
    let raw_count = |state: &ToolHistoryState| {
        state
            .project(Arc::clone(&items))
            .items
            .iter()
            .filter_map(textual_output_identity)
            .filter(|(_, text)| *text == output)
            .count()
    };
    state.set_model_visible_tool_result_token_budget(Some(3_000));
    assert!(raw_count(&state) < 8, "a small window budget must compact");
    state.set_model_visible_tool_result_token_budget(Some(129_200));
    assert_eq!(
        raw_count(&state),
        8,
        "a large window keeps the evidence raw"
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
    let receipt = serde_json::json!({"runner":"rust_test_runner", "selected_packages":["app"],
        "dependency_manifest":{"source_roots":[temp.path()]}});
    assert_eq!(runner_receipt_dependencies(&receipt, temp.path()), dependencies);
    assert!(runner_receipt_dependencies(&serde_json::json!({"runner":"rust_test_runner",
        "selected_packages":["missing"], "dependency_manifest":{"source_roots":[temp.path()]}}), temp.path()).is_empty());
    let package_index = cargo_package_index(temp.path());
    assert!(
        dependencies.contains(&SourceDependencyV1::new(&temp.path().join("app"), true,)),
        "selected package graph: {dependencies:#?}; package index: {package_index:#?}"
    );
    assert!(dependencies.contains(&SourceDependencyV1::new(&temp.path().join("support"), true,)));
    assert!(!dependencies.contains(&SourceDependencyV1::new(temp.path(), true)));
    for arguments in [
        serde_json::json!({"program":"cargo", "args":["test", "-p", "app"]}),
        serde_json::json!({"cmd":"cargo nextest run --package=app"}),
        serde_json::json!({"program":"cargo", "args":["test", "-papp", "--", "--package=ignored-test-arg"]}),
    ] {
        let payload = ToolPayload::Function { arguments: arguments.to_string() };
        assert_eq!(source_dependencies_for_tool_call("exec_command", &payload, temp.path()), dependencies);
    }
    let runner = serde_json::from_value(serde_json::json!({
        "programs":["python"], "prefixes":[["run-lane"]], "passthrough_after":"--",
        "allow_extra_args":true,
    })).unwrap();
    let command = ["python", "run-lane", "--", "cargo", "test", "-p", "app"].map(str::to_owned);
    assert_eq!(runner_source_dependencies(&command, temp.path(), &[runner]), Some(dependencies));
    for args in [vec!["test"], vec!["test", "--workspace", "-p", "app"],
        vec!["test", "--manifest-path", "other/Cargo.toml", "-p", "app"],
        vec!["test", "-p", "unknown"]] {
        assert!(cargo_argv_dependencies(&args.into_iter().map(str::to_owned).collect::<Vec<_>>(), temp.path()).is_empty());
    }
}

#[test]
fn cargo_configuration_changes_invalidate_observed_validation() {
    let temp = tempfile::tempdir().expect("workspace fixture");
    let workspace = temp.path().join("workspace");
    let app = workspace.join("app");
    std::fs::create_dir_all(&app).expect("package directory");
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = ['app']\n",
    )
    .expect("workspace manifest");
    std::fs::write(
        app.join("Cargo.toml"),
        "[package]\nname = 'app'\nversion = '0.1.0'\n",
    )
    .expect("package manifest");
    let arguments = serde_json::json!({"package": "app", "workdir": app});
    let payload = ToolPayload::Function {
        arguments: arguments.to_string(),
    };
    let classification = classify_workspace_tool_call("cargo_test", &payload, &app);
    assert!(classification.observes_workspace);
    let call_id = "cargo-proof";
    let output = text_output(call_id, "test result: ok. 1 passed".to_string());
    let canonical: Arc<[ResponseItem]> = Arc::from([
        named_function_call_with_arguments(call_id, "cargo_test", arguments),
        output.clone(),
    ]);
    for directory in [workspace.as_path(), temp.path()] {
        for input in [
            ".cargo/config",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
        ] {
            let path = directory.join(input);
            assert!(!path.exists(), "track configuration before it is created");
            let mut state = ToolHistoryState::default();
            state.register_workspace_evidence(
                WorkspaceEvidenceObservation::from_response_item(
                    Some(workspace_identity("captured")),
                    &output,
                    classification.source_dependencies.clone(),
                )
                .expect("validation observation"),
            );
            let unrelated = workspace_identity("unrelated");
            state.invalidate_source_dependencies(
                Some(&BTreeSet::from([workspace.join("notes.txt")])),
                Some(&unrelated),
            );
            assert_eq!(
                state
                    .project_with_workspace_identity(Arc::clone(&canonical), Some(&unrelated))
                    .items,
                canonical,
                "unrelated changes must preserve the validation result"
            );
            let changed = workspace_identity("configuration-changed");
            state.invalidate_source_dependencies(
                Some(&BTreeSet::from([path.clone()])),
                Some(&changed),
            );
            let projected =
                state.project_with_workspace_identity(Arc::clone(&canonical), Some(&changed));
            let (_, text) = textual_output_identity(&projected.items[1]).expect("stale result");
            let notice: serde_json::Value =
                serde_json::from_str(text).expect("stale evidence notice");
            assert_eq!(notice["stale_workspace_evidence"], true, "{path:?}");
            assert_eq!(
                notice["reason_code"], "source_dependencies_invalidated",
                "{path:?}"
            );
            assert!(notice["rerun"].get("force_fresh").is_none(), "{path:?}");
        }
    }
}

#[test]
fn verified_named_runner_scope_uses_manifest_target_package_graph() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("codex-rs");
    std::fs::create_dir_all(workspace.join(".config")).unwrap();
    std::fs::create_dir_all(workspace.join("app")).unwrap();
    std::fs::create_dir_all(workspace.join("tui")).unwrap();
    std::fs::write(workspace.join("Cargo.toml"), "[workspace]\nmembers=[\"app\",\"tui\"]\n").unwrap();
    for name in ["app", "tui"] {
        std::fs::write(workspace.join(name).join("Cargo.toml"), format!("[package]\nname=\"{name}\"\nversion=\"0.1.0\"\n")).unwrap();
    }
    let manifest = workspace.join(".config/kd4-rust-tests.toml");
    std::fs::write(&manifest, "version=1\n[targets.app_lib]\npackage=\"app\"\n[gates.small]\nsteps=[{target=\"app_lib\"}]\n").unwrap();
    let mut runner: codex_shell_command::validation::RepositoryRunner = serde_json::from_value(serde_json::json!({
        "programs":["python"], "prefixes":[["scripts/rust_test_runner.py", "run-target"],["scripts/rust_test_runner.py", "run-gate"]],
        "operations":["test"], "receipt_runner":"rust_test_runner", "allow_extra_args":true,
    })).unwrap();
    runner.path_context = Some((root.path().to_path_buf(), root.path().to_path_buf()));
    for (operation, name) in [("run-target", "app_lib"), ("run-gate", "small")] {
        let command = ["python", "scripts/rust_test_runner.py", operation, name].map(str::to_owned);
        let scope = runner_source_dependencies(&command, root.path(), &[runner.clone()]).unwrap();
        assert!(scope.contains(&SourceDependencyV1::new(&workspace.join("app"), true)));
        assert!(!scope.contains(&SourceDependencyV1::new(&workspace.join("tui"), true)));
        assert!(scope.contains(&SourceDependencyV1::new(&manifest, false)));
    }
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
    assert!(collect_cargo_package_dependencies(
        &app_root,
        &index,
        &mut visited,
        &mut dependencies
    ));
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
fn workspace_evidence_observes_uncertain_commands_and_scopes_powershell_reads() {
    for command in ["Get-Content contract.txt", "custom-inspector src"] {
        let payload = ToolPayload::Function {
            arguments: serde_json::json!({"kind": "script", "cmd": command}).to_string(),
        };
        let classification =
            classify_workspace_tool_call("exec_command", &payload, Path::new("/repo"));
        assert!(classification.observes_workspace, "{command}");
        if command.starts_with("Get-Content") {
            assert_eq!(
                classification.source_dependencies,
                BTreeSet::from([SourceDependencyV1::new(
                    Path::new("/repo/contract.txt"),
                    false
                )])
            );
        }
    }
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
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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

    let (fallback_call_id, fallback_output) =
        textual_output_identity(&projection.unreplaced_items[0])
            .expect("fallback must retain a recovery handle");
    let pin: serde_json::Value =
        serde_json::from_str(fallback_output).expect("fallback artifact pin");
    assert_eq!(fallback_call_id, call_id);
    assert_eq!(pin["kind"], "tool_history_artifact_pin");
    assert_eq!(pin["artifact_id"], "artifact-1");
    assert_eq!(pin["bytes"], 96_000);
    assert_eq!(pin["sha256"], sha256(b"canonical artifact"));
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
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
fn tool_history_receipt_does_not_replace_output_when_only_an_empty_digest_fits() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    // This call ID leaves exactly enough envelope space for an empty digest.
    // Such a receipt would fail the consumer's nonempty-digest validation.
    let bounded = bounded_output();
    let sample = candidate("", bounded.clone());
    let mut empty: ToolHistoryReceiptV2 =
        serde_json::from_str(sample.admission_receipt().unwrap().1).unwrap();
    empty.digest.clear();
    let envelope_bytes = serde_json::to_vec(&empty).unwrap().len();
    let call_id = "c".repeat(
        codex_utils_string::approx_bytes_for_tokens(RECEIPT_MAX_TOKENS) - envelope_bytes,
    );
    let canonical: Arc<[ResponseItem]> = Arc::from([text_output(&call_id, bounded.clone())]);
    let tracked = candidate(&call_id, bounded);
    assert!(tracked.admission_receipt().is_none());
    let mut state = ToolHistoryState::default();
    state.register(tracked);
    assert!(state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));

    let projection = state.project(Arc::clone(&canonical));
    assert!(projection.substitutions.is_empty());
    let (projected_call_id, output) =
        textual_output_identity(&projection.items[0]).expect("artifact recovery output");
    let pin: serde_json::Value = serde_json::from_str(output).expect("artifact pin JSON");
    assert_eq!(projected_call_id, call_id);
    assert_eq!(pin["kind"], "tool_history_artifact_pin");
    assert_eq!(pin["artifact_id"], "artifact-1");
}

#[test]
fn tool_history_receipt_requires_nonempty_model_output() {
    let tracked = candidate("empty-output", String::new());
    assert!(tracked.admission_receipt().is_none());
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
fn tool_history_admission_keeps_first_exposure_receipt_detail_unresolved() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
    assert_eq!(projection.unreplaced_items.len(), 2);
    assert!(
        projection
            .unreplaced_items
            .iter()
            .filter_map(textual_output_identity)
            .map(|(_, output)| approx_token_count(output))
            .sum::<usize>()
            <= model_visible_tool_result_token_budget()
    );
    assert_eq!(
        projection
            .unreplaced_items
            .iter()
            .filter_map(output_call_id)
            .collect::<Vec<_>>(),
        vec!["call-1", "call-2"]
    );
    assert!(
        projection
            .items
            .iter()
            .filter_map(textual_output_identity)
            .map(|(_, output)| approx_token_count(output))
            .sum::<usize>()
            <= model_visible_tool_result_token_budget()
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
    assert_eq!(state.consumed_outputs_for_tool("functions.exec").len(), 1);
    let unread = state.candidates.values().find(|candidate| candidate.consumed_by_generation.is_none()).unwrap().call_id.clone();
    let _pressure = override_model_visible_tool_result_token_budget_for_test(1);
    let consumed = state.project(canonical);
    let rendered = serde_json::to_string(&consumed.items).unwrap();
    assert!(rendered.contains("unread outcomes are unresolved"));
    assert!(rendered.contains(&unread));
    assert!(rendered.contains("artifact-1"));
}

#[test]
fn tool_history_recovery_handles_cannot_bypass_the_aggregate_budget() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for index in 0..160 {
        let call_id = format!("call-{index:03}");
        let output = format!("result-{index} {}", "evidence ".repeat(500));
        let mut tracked = candidate(&call_id, output.clone());
        tracked.artifact_id = format!("artifact-{index:03}");
        tracked.refresh_derived();
        state.register(tracked);
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, output));
    }
    let canonical: Arc<[ResponseItem]> = Arc::from(items);
    let before = serde_json::to_vec(&canonical).unwrap();
    let first = state.project(Arc::clone(&canonical));
    // Replaying already-projected receipts must not bypass admission's original-output hash.
    let replay = state.project(Arc::clone(&first.items));
    for items in [
        &first.items,
        &first.unreplaced_items,
        &replay.items,
        &replay.unreplaced_items,
    ] {
        let outputs = items
            .iter()
            .filter_map(textual_output_identity)
            .collect::<Vec<_>>();
        assert!(
            outputs
                .iter()
                .map(|(_, text)| approx_token_count(text))
                .sum::<usize>()
                <= model_visible_tool_result_token_budget()
        );
        assert!(!outputs.is_empty());
        assert!(
            outputs.len() < 160,
            "fixture must exhaust even the recovery-handle budget"
        );
        assert!(outputs.iter().any(|(id, _)| *id == "call-159"));
        for item in items.iter() {
            if let ResponseItem::FunctionCall { call_id, .. } = item {
                assert!(outputs.iter().any(|(id, _)| *id == call_id));
            }
        }
    }
    assert_eq!(serde_json::to_vec(&canonical).unwrap(), before);
    assert_eq!(
        state.candidates.len(),
        160,
        "projection must not delete saved recovery state"
    );
    for substitution in first.substitutions.iter() {
        assert_eq!(
            output_call_id(&first.items[substitution.item_index]),
            Some(substitution.call_id.as_str())
        );
    }
}

#[test]
fn final_budget_reserves_recovery_before_admitting_untracked_raw_output() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(1_000);
    let mut state = ToolHistoryState::default();
    let old_output = "prior source evidence\n".repeat(400);
    let mut tracked = candidate("prior-read", old_output.clone());
    tracked.consumed_by_generation = Some(ModelGenerationId {
        turn_id: "current-turn".into(),
        ordinal: 0,
    });
    tracked.refresh_derived();
    let pin = tracked.artifact_pin().unwrap();
    state.register(tracked);
    // This result has no history candidate (as with dispatch/continuation
    // outcomes). It fits alone, but must not consume the older recovery handle.
    let recent_output = "r".repeat(3_800);
    assert!(approx_token_count(&recent_output) <= 1_000);
    assert!(approx_token_count(&recent_output) + pin.1 > 1_000);
    let canonical: Arc<[ResponseItem]> = Arc::from(vec![
        function_call("prior-read"),
        text_output("prior-read", old_output),
        function_call("recent-outcome"),
        text_output("recent-outcome", recent_output),
    ]);

    let projection = state.project(Arc::clone(&canonical));
    for items in [&projection.items, &projection.unreplaced_items] {
        let outputs = items
            .iter()
            .filter_map(canonical_textual_output_identity)
            .collect::<Vec<_>>();
        assert_eq!(
            outputs.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            ["prior-read", "recent-outcome"],
            "raw output must not displace recovery when both receipts fit"
        );
        assert!(
            outputs
                .iter()
                .map(|(_, text)| approx_token_count(text))
                .sum::<usize>()
                <= 1_000
        );
        let prior: serde_json::Value = serde_json::from_str(&outputs[0].1).unwrap();
        assert_eq!(prior["artifact_id"], "artifact-1");
        let recent: serde_json::Value = serde_json::from_str(&outputs[1].1).unwrap();
        assert_eq!(recent["kind"], "unconsumed_tool_outcome");
        assert_eq!(recent["output_omitted"], true);
        assert_eq!(
            items
                .iter()
                .filter(|item| item_call_id(item).is_some())
                .count(),
            4
        );
    }
    assert_eq!(projection.items_budget_drops.count, 0);
    assert_eq!(projection.unreplaced_items_budget_drops.count, 0);
    assert_eq!(canonical.len(), 4);
}

#[test]
fn continuation_projection_reuses_unmodified_prepared_storage() {
    let workspace = GitWorkspaceCache::with_noop_watcher_for_tests();
    let mut state = ToolHistoryState::default();
    let mut items: Arc<[ResponseItem]> = Arc::from([]);
    let mut anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&items),
        projection: ToolHistoryProjection {
            items: Arc::clone(&items),
            unreplaced_items: Arc::clone(&items),
            ..Default::default()
        },
    };
    for index in 0..3 {
        let call_id = format!("shared-{index}");
        state.register_non_workspace_code_mode_call(call_id.clone());
        let mut extended = items.to_vec();
        extended.extend([function_call(&call_id), text_output(&call_id, "result".into())]);
        items = extended.into();
        let projection = state.project_continuation_with_workspace_cache(
            &anchor, Arc::clone(&items), None, &workspace,
        ).unwrap();
        assert!(Arc::ptr_eq(&projection.items, &items));
        assert!(Arc::ptr_eq(&projection.unreplaced_items, &items));
        anchor = SamplingProjectionAnchor { prepared_items: Arc::clone(&items), projection };
    }
    let retry = state.project_continuation_with_workspace_cache(
        &anchor, Arc::clone(&items), None, &workspace,
    ).unwrap();
    assert!(Arc::ptr_eq(&retry.items, &items));

    // A distinct primary projection must still be extended, while its unchanged
    // unreplaced representation can independently reuse the prepared input.
    let mut projected = items.to_vec();
    projected[1] = text_output("shared-0", "projected result".into());
    anchor.projection.items = projected.into();
    state.register_non_workspace_code_mode_call("last".into());
    let mut extended = items.to_vec();
    extended.extend([function_call("last"), text_output("last", "last result".into())]);
    let extended: Arc<[ResponseItem]> = extended.into();
    let continued = state.project_continuation_with_workspace_cache(
        &anchor, Arc::clone(&extended), None, &workspace,
    ).unwrap();
    assert!(continued.items.starts_with(&anchor.projection.items));
    assert!(!Arc::ptr_eq(&continued.items, &extended));
    assert!(Arc::ptr_eq(&continued.unreplaced_items, &extended));
}

#[test]
fn continuation_projection_keeps_the_previous_request_as_a_prefix() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    let workspace = crate::git_workspace::GitWorkspaceCache::new();
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    let evidence = |index: usize| format!("result-{index} {}", "evidence ".repeat(500));
    for index in 0..20 {
        let call_id = format!("saved-{index:03}");
        let mut tracked = candidate(&call_id, evidence(index));
        tracked.artifact_id = format!("artifact-{index:03}");
        tracked.refresh_derived();
        state.register(tracked);
        // These synthetic results have no workspace dependencies; classify them
        // as dispatch does so freshness validation leaves their text alone and
        // only the aggregate budget shapes the projection.
        state.register_non_workspace_code_mode_call(call_id.clone());
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, evidence(index)));
    }
    let canonical: Arc<[ResponseItem]> = Arc::from(items.clone());
    let first = state.project_with_workspace_cache(Arc::clone(&canonical), None, &workspace);
    assert_ne!(
        &first.items[..],
        &canonical[..],
        "fixture must exceed the aggregate budget so the projection is not the raw history"
    );
    let anchor = SamplingProjectionAnchor {
        prepared_items: Arc::clone(&canonical),
        projection: first.clone(),
    };

    // The model observed that request; the next request appends one more result.
    // Re-running the budget now demotes the observed outputs behind the new one,
    // which is exactly the prefix churn a continuation must not produce.
    let _ = state.mark_consumed_with_delta(
        &canonical,
        ModelGenerationId {
            turn_id: "turn".into(),
            ordinal: 1,
        },
    );
    let mut newest = candidate("saved-020", evidence(20));
    newest.artifact_id = "artifact-020".to_string();
    newest.refresh_derived();
    state.register(newest);
    state.register_non_workspace_code_mode_call("saved-020".to_string());
    let appended = [
        function_call("saved-020"),
        text_output("saved-020", evidence(20)),
    ];
    items.extend(appended.iter().cloned());
    let extended: Arc<[ResponseItem]> = Arc::from(items);

    let continued = state
        .project_continuation_with_workspace_cache(&anchor, Arc::clone(&extended), None, &workspace)
        .expect("the anchored request prefixes the extended history");
    assert_eq!(
        &continued.items[..first.items.len()],
        &first.items[..],
        "items the model already received must keep their exact representation"
    );
    assert_eq!(&continued.items[first.items.len()..], &appended[..]);
    assert_eq!(
        &continued.unreplaced_items[..first.unreplaced_items.len()],
        &first.unreplaced_items[..]
    );
    assert_eq!(continued.substitutions, first.substitutions);
    assert_eq!(continued.items_budget_drops, first.items_budget_drops);
    assert_eq!(
        continued.unreplaced_items_budget_drops,
        first.unreplaced_items_budget_drops
    );

    // A rewritten or shorter history is not a continuation of that request.
    let mut diverged = extended.to_vec();
    diverged[0] = function_call("replaced-000");
    assert!(
        state
            .project_continuation_with_workspace_cache(
                &anchor,
                Arc::from(diverged),
                None,
                &workspace
            )
            .is_none()
    );
    assert!(
        state
            .project_continuation_with_workspace_cache(
                &anchor,
                Arc::from(&canonical[..2]),
                None,
                &workspace
            )
            .is_none()
    );
}

#[test]
fn aggregate_budget_drops_are_recorded_per_representation() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for index in 0..20 {
        let call_id = format!("saved-{index:03}");
        let output = format!("result-{index} {}", "evidence ".repeat(500));
        let mut tracked = candidate(&call_id, output.clone());
        tracked.artifact_id = format!("artifact-{index:03}");
        tracked.consumed_by_generation = Some(ModelGenerationId {
            turn_id: "prior-turn".into(),
            ordinal: 0,
        });
        tracked.refresh_derived();
        state.register(tracked);
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, output));
    }
    // Short untracked outcomes are already cheaper than a compact receipt.
    // Enough of them must still exhaust both representations' aggregate budget.
    for index in 0..130 {
        let call_id = format!("unsaved-{index:03}");
        items.push(function_call(&call_id));
        items.push(text_output(
            &call_id,
            format!("unsaved-{index} {}", "pending ".repeat(30)),
        ));
    }
    let canonical: Arc<[ResponseItem]> = Arc::from(items);

    // The budget is one of several stages that can remove an output, so its
    // own drops are measured where it runs.
    let outputs = |items: &ProjectedResponseItems| {
        items
            .iter()
            .filter_map(textual_output_identity)
            .map(|(call_id, text)| (call_id.to_string(), approx_token_count(text)))
            .collect::<Vec<_>>()
    };
    let mut budgeted = ProjectedResponseItems::Shared(Arc::clone(&canonical));
    let before = outputs(&budgeted);
    let drops = state.enforce_tool_result_budget(&mut budgeted);
    let after = outputs(&budgeted);
    assert!(
        !after.is_empty() && after.len() < before.len(),
        "fixture must exhaust the aggregate budget without emptying history"
    );
    assert_eq!(
        drops.count as usize,
        before.len() - after.len(),
        "the recorded count must match the results the budget removed"
    );
    assert_eq!(
        drops.tokens,
        before
            .iter()
            .filter(|(call_id, _)| !after.iter().any(|(kept, _)| kept == call_id))
            .map(|(_, tokens)| *tokens as u64)
            .sum::<u64>(),
        "the recorded tokens must be what the dropped results would have cost"
    );

    // Both representations carry their own numbers into the projection. They
    // present different output sizes to the same budget, so one shared number
    // could not describe both.
    let projection = state.project(Arc::clone(&canonical));
    assert!(projection.items_budget_drops.count > 0);
    assert!(projection.items_budget_drops.tokens > 0);
    assert_ne!(
        projection.items_budget_drops, projection.unreplaced_items_budget_drops,
        "each representation must be measured on what it actually sends"
    );

    // A history that fits records nothing, so a request that sends it reports
    // no drop delta.
    let small = state.project(Arc::from(vec![
        function_call("call-000"),
        text_output("call-000", "short".to_string()),
    ]));
    assert_eq!(
        small.items_budget_drops,
        crate::tool_history::ToolOutputBudgetDrops::default()
    );
    assert_eq!(
        small.unreplaced_items_budget_drops,
        crate::tool_history::ToolOutputBudgetDrops::default()
    );
}

#[tokio::test]
async fn remote_compaction_keeps_recovery_for_evidence_omitted_by_provider() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let call_id = "failed-validation";
    let output = "FAILED: assertion expected 2, got 1".to_string();
    let mut tracked = candidate(call_id, output.clone());
    tracked.successful = false;
    tracked.semantic_class = "validation".to_string();
    session.register_tool_history_candidate(tracked).await;
    session
        .record_conversation_items(
            &turn,
            &[function_call(call_id), text_output(call_id, output)],
        )
        .await
        .unwrap();
    let opaque = ResponseItem::Compaction {
        id: None,
        encrypted_content: "provider omitted tool output".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let (installed, _, _) = crate::compact_remote::process_compacted_history(
        &session,
        &turn,
        vec![opaque.clone()],
        &crate::compact::InitialContextInjection::DoNotInject,
    )
    .await;
    assert_eq!(installed[0], opaque);
    let ResponseItem::Message { content, .. } = &installed[1] else {
        panic!("expected durable recovery metadata");
    };
    let codex_protocol::models::ContentItem::InputText { text } = &content[0] else {
        panic!("expected artifact pins");
    };
    let pins: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(pins["kind"], "tool_history_artifact_pins");
    assert_eq!(pins["artifacts"][0]["artifact_id"], "artifact-1");
    assert_eq!(
        pins["artifacts"][0]["sha256"],
        sha256(b"canonical artifact")
    );
    assert_eq!(pins["artifacts"][0]["bytes"], 96_000);
}

#[tokio::test]
async fn remote_compaction_bounds_recovery_metadata_and_keeps_newest_exact_handles() {
    let (session, turn_context) = crate::session::tests::make_session_and_context().await;
    let mut items = Vec::new();
    for index in 0..96 {
        // Reverse lexical ordering to distinguish recency from the candidate map's key order.
        let call_id = format!("call-{:03}", 96 - index);
        let mut tracked = candidate(&call_id, format!("output-{index}"));
        tracked.artifact_id = format!("artifact-{index:03}");
        tracked.refresh_derived();
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, tracked.artifact_pin().unwrap().0));
        session.register_tool_history_candidate(tracked).await;
    }
    items.push(ResponseItem::Compaction {
        id: None,
        encrypted_content: "opaque-state".to_string(),
        internal_chat_message_metadata_passthrough: None,
    });
    let (installed, _, _) = crate::compact_remote::process_compacted_history(
        &session,
        &turn_context,
        items,
        &crate::compact::InitialContextInjection::DoNotInject,
    )
    .await;
    let sidecar = installed
        .iter()
        .find_map(|item| {
            let ResponseItem::Message { content, .. } = item else {
                return None;
            };
            content.iter().find_map(|content| {
                let codex_protocol::models::ContentItem::InputText { text } = content else {
                    return None;
                };
                let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
                (value["kind"] == "tool_history_artifact_pins").then_some((text, value))
            })
        })
        .expect("installed compaction must carry the bounded recovery metadata");
    assert!(approx_token_count(sidecar.0) <= 2_000);
    let pins = sidecar.1["artifacts"].as_array().unwrap();
    assert!(!pins.is_empty());
    assert!(pins.len() <= 32);
    assert_eq!(pins[0]["artifact_id"], "artifact-095");
    assert_eq!(pins[0]["bytes"], 96_000);
    assert_eq!(pins[0]["sha256"], sha256(b"canonical artifact"));
    assert_eq!(sidecar.1["omitted_artifact_count"], 96 - pins.len());
    assert!(
        sidecar.1["instruction"]
            .as_str()
            .unwrap()
            .contains("read_tool_output")
    );
    assert!(pins.iter().all(|pin| pin.get("retrieval").is_none()));
    // Compact pins must remain recognized as exact artifact references when
    // this sidecar is the only recovery metadata retained in history.
    let mut state = ToolHistoryState::default();
    let mut tracked = candidate("newest", "output-95".to_string());
    tracked.artifact_id = "artifact-095".to_string();
    tracked.refresh_derived();
    state.register(tracked);
    assert_eq!(
        state.artifact_reference_positions(&[text_output("sidecar", sidecar.0.clone())])
            .into_iter()
            .map(|(call_id, (std::cmp::Reverse(index), _))| (call_id, index))
            .collect::<BTreeMap<_, _>>(),
        BTreeMap::from([("newest".to_string(), 0)])
    );
}

#[test]
fn tool_history_admission_reserves_competing_results_before_spending_the_shared_budget() {
    let older = "older ".repeat(1_000);
    let newest = "abc ".repeat(model_visible_tool_result_token_budget());
    assert_eq!(
        approx_token_count(&newest),
        model_visible_tool_result_token_budget()
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
            <= model_visible_tool_result_token_budget()
    );
}

#[test_case::test_case(20_000; "oversized results")]
#[test_case::test_case(1_000; "individually fitting results")]
fn token_backfire_tool_history_pressure_keeps_every_result_recoverable(repetitions: usize) {
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for index in 0..80 {
        let call_id = format!("call-{index}");
        let output = format!("result-{index} ").repeat(repetitions);
        let mut registered = candidate(&call_id, output.clone());
        registered.artifact_id = format!("00000000-0000-7000-8000-{index:012}");
        state.register(registered);
        items.push(function_call(&call_id));
        items.push(text_output(&call_id, output));
    }

    let projection = state.project(Arc::from(items));

    for items in [&projection.items, &projection.unreplaced_items] {
        assert_eq!(items.len(), 160);
        let outputs = items
            .iter()
            .filter_map(textual_output_identity)
            .collect::<Vec<_>>();
        assert!(
            outputs
                .iter()
                .map(|(_, output)| approx_token_count(output))
                .sum::<usize>()
                <= model_visible_tool_result_token_budget(),
            "recoverability must not increase the aggregate token budget"
        );
        for index in 0..80 {
            let call_id = format!("call-{index}");
            let output = outputs
                .iter()
                .find(|(projected_call_id, _)| *projected_call_id == call_id.as_str())
                .map(|(_, output)| *output)
                .expect(
                    "every result must retain a model-visible recovery path in both projections",
                );
            if !output.starts_with(&format!("result-{index} ")) {
                let recovery: serde_json::Value = serde_json::from_str(output).unwrap();
                assert_eq!(
                    recovery["artifact_id"],
                    format!("00000000-0000-7000-8000-{index:012}")
                );
            }
        }
    }
}

#[test]
fn tool_history_admission_preserves_newest_unconsumed_non_text_content() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    let call_id = "image-call";
    let output = "small text".to_string();
    let images = vec![FunctionCallOutputContentItem::InputImage {
        image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
        detail: None,
    }; 300];
    let mut image_candidate = candidate(call_id, output.clone());
    let non_text_tokens =
        approx_token_count(&serde_json::to_string(&images).expect("serialize image content"))
            as u64;
    assert!(non_text_tokens > model_visible_tool_result_token_budget() as u64);
    image_candidate.preserved_non_text_tokens = Some(non_text_tokens);
    let mut content = vec![FunctionCallOutputContentItem::InputText { text: output }];
    content.extend(images);
    let canonical: Arc<[ResponseItem]> = Arc::from([
        function_call(call_id),
        ResponseItem::FunctionCallOutput {
            id: None,
            call_id: call_id.to_string(),
            output: FunctionCallOutputPayload::from_content_items(content),
            internal_chat_message_metadata_passthrough: None,
        },
    ]);
    let mut state = ToolHistoryState::default();
    state.register(image_candidate);

    let projection = state.project(Arc::clone(&canonical));

    assert_eq!(projection.items, canonical);
    assert_eq!(projection.unreplaced_items, canonical);
    assert!(projection.substitutions.is_empty());
}

#[test]
fn tool_history_keeps_consumed_contract_details_when_all_results_fit() {
    let call_id = "read-contract";
    let output = format!(
        "{}\nReject non-ASCII input with ValueError; inventory total must be 5.",
        "Source and contract details needed for the subsequent edit.\n".repeat(60)
    );
    let canonical: Arc<[ResponseItem]> = Arc::from([
        function_call(call_id),
        text_output(call_id, output.clone()),
        named_function_call("read-tests", "non_workspace_operation"),
        text_output(
            "read-tests",
            "Three tests exercise the contract.".to_string(),
        ),
    ]);
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, output.clone()));
    assert_eq!(state.project(Arc::clone(&canonical)).items, canonical);
    assert!(state.mark_consumed(
        &canonical,
        ModelGenerationId {
            turn_id: "turn-1".to_string(),
            ordinal: 1,
        },
    ));

    let projection = state.project(Arc::clone(&canonical));
    assert_eq!(projection.items, canonical);
    assert_eq!(
        textual_output_identity(&projection.items[1]),
        Some((call_id, output.as_str()))
    );
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
    let output = "abc ".repeat(MINIMUM_RAW_TOKENS as usize);
    let raw_tokens = approx_token_count(&output);
    assert_eq!(raw_tokens, MINIMUM_RAW_TOKENS as usize);
    assert!(raw_tokens <= model_visible_tool_result_token_budget());
    let call_id = (1..64)
        .map(|length| "threshold-call".repeat(length))
        .find(|call_id| {
            candidate(call_id, output.clone())
                .admission_receipt()
                .is_some_and(|(_, _, tokens)| {
                    tokens <= raw_tokens as u64
                        && (raw_tokens as u64 - tokens) < MINIMUM_SAVED_TOKENS
                })
        })
        .expect("fixture leaves less than the minimum savings");
    let call_id = call_id.as_str();

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
    assert!(Arc::ptr_eq(&projection.items, &canonical));
    assert!(Arc::ptr_eq(&projection.unreplaced_items, &canonical));
    assert!(projection.substitutions.is_empty());
}

#[test]
fn tool_history_admission_prioritizes_plain_failure_outputs() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
    assert_eq!(projection.unreplaced_items.len(), 1);
    assert!(
        matches!(&projection.unreplaced_items[0], ResponseItem::Message { role, content, .. }
        if role == "developer" && format!("{content:?}").contains("2 unread outcomes"))
    );
}

#[test]
fn tool_search_receipt_retains_largest_fitting_identity_prefix() {
    for count in [0, 1, 10, 99, 256] {
        let mut item = tool_search_pair("prefix", 0)[1].clone();
        let ResponseItem::ToolSearchOutput { tools, .. } = &mut item else {
            unreachable!()
        };
        *tools = (0..count)
            .map(|index| {
                serde_json::json!({
                    "name": format!("tool_{index}_{}", "long_name".repeat(index % 7)),
                })
            })
            .collect();
        let identities = tools
            .iter()
            .filter_map(tool_search_result_identity)
            .collect::<Vec<_>>();
        let (result, tokens) = tool_search_receipt_item(&item, None).expect("receipt fits");
        let receipt = tool_search_receipt(&result).unwrap();
        let retained = receipt.ordered_tool_identities.len();
        assert_eq!(receipt.ordered_tool_identities, identities[..retained]);
        assert_eq!(receipt.omitted_identity_count, count - retained);
        assert_eq!(
            tokens,
            approx_token_count(&serde_json::to_string(&result).unwrap())
        );
        assert!(tokens <= TOOL_SEARCH_RECEIPT_ENVELOPE_MAX_TOKENS);
        assert!(
            approx_token_count(&serde_json::to_string(&receipt).unwrap()) <= RECEIPT_MAX_TOKENS
        );
        // Every longer prefix must fail at least one of the two independent ceilings.
        for length in retained + 1..=count {
            let mut larger = receipt.clone();
            larger.ordered_tool_identities = identities[..length].to_vec();
            larger.omitted_identity_count = count - length;
            larger.receipt_id = tool_search_receipt_id(
                &larger.call_id,
                &larger.status,
                &larger.execution,
                &larger.arguments,
                &larger.result_set_sha256,
                larger.result_count,
                larger.omitted_result_count,
                larger.complete,
                larger.omitted_identity_count,
                &larger.ordered_tool_identities,
            );
            let mut envelope = result.clone();
            let ResponseItem::ToolSearchOutput { tools, .. } = &mut envelope else {
                unreachable!()
            };
            *tools = vec![serde_json::json!({"type": "tool_search_receipt", "receipt": larger})];
            assert!(
                approx_token_count(&serde_json::to_string(&larger).unwrap()) > RECEIPT_MAX_TOKENS
                    || approx_token_count(&serde_json::to_string(&envelope).unwrap())
                        > TOOL_SEARCH_RECEIPT_ENVELOPE_MAX_TOKENS
            );
        }
    }
}

#[test]
fn tool_search_receipt_caps_all_argument_fields_and_binds_semantics() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
    let original_arguments = arguments.clone();

    let projection = ToolHistoryState::default().project(Arc::from(pair));
    let (receipt_item, receipt) = projection
        .items
        .iter()
        .find_map(|item| tool_search_receipt(item).map(|receipt| (item, receipt)))
        .expect("bounded search receipt");
    let rendered_item = serde_json::to_string(receipt_item).expect("serialize receipt envelope");
    assert!(approx_token_count(&rendered_item) <= TOOL_SEARCH_RECEIPT_ENVELOPE_MAX_TOKENS);
    let rendered = serde_json::to_string(&receipt).expect("serialize receipt");
    assert!(approx_token_count(&rendered) <= RECEIPT_MAX_TOKENS);
    for key in ["query", "namespace", "limit", "cursor"] {
        assert_eq!(
            receipt.arguments[format!("{key}_sha256")],
            sha256(original_arguments[key].to_string().as_bytes())
        );
    }

    // Size the fixture from the actual envelope so it exceeds the global budget.
    // The normal admission path must instead charge the complete model-visible outputs.
    let pressure_count =
        model_visible_tool_result_token_budget() / approx_token_count(&rendered_item) + 1;
    let pressure_items = (0..pressure_count)
        .flat_map(|index| {
            let mut pair = tool_search_pair(&format!("search-{index}"), 48_000);
            let ResponseItem::ToolSearchCall { arguments, .. } = &mut pair[0] else {
                panic!("expected search call");
            };
            *arguments = original_arguments.clone();
            pair
        })
        .collect::<Vec<_>>();
    let pressured = ToolHistoryState::default().project(Arc::from(pressure_items));
    let receipts = pressured
        .items
        .iter()
        .filter(|item| matches!(item, ResponseItem::ToolSearchOutput { .. }))
        .collect::<Vec<_>>();
    assert!(!receipts.is_empty());
    assert!(receipts.len() < pressure_count);
    let total_output_tokens = receipts
        .into_iter()
        .map(|item| {
            let receipt = tool_search_receipt(item).expect("retained search output is a receipt");
            assert!(
                approx_token_count(&serde_json::to_string(&receipt).unwrap()) <= RECEIPT_MAX_TOKENS
            );
            let tokens = approx_token_count(&serde_json::to_string(item).unwrap());
            assert!(tokens <= TOOL_SEARCH_RECEIPT_ENVELOPE_MAX_TOKENS);
            tokens
        })
        .sum::<usize>();
    assert!(total_output_tokens <= model_visible_tool_result_token_budget());

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
            &changed.ordered_tool_identities,
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
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
    assert_eq!(projection.unreplaced_items.len(), 2);
    assert!(
        projection
            .unreplaced_items
            .iter()
            .all(|item| item_call_id(item) == Some("call-2"))
    );
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
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    let call_id = "mcp-multi-text";
    let first = "first section\n".repeat(2_000);
    let second = "second section\n".repeat(2_000);
    let bounded = format!("{first}\n{second}");
    assert!(approx_token_count(&bounded) > model_visible_tool_result_token_budget());
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
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
        consumption_turns: Vec::new(),
        candidates: BTreeMap::from([("call-1".to_string(), candidate("call-1", bounded))]),
        untracked_consumption: BTreeMap::new(),
        exposed_representations: BTreeMap::new(),
        recovered_call_ids: BTreeSet::new(),
        recovered_ranges: BTreeMap::new(),
        workspace_evidence: BTreeMap::new(),
        non_workspace_code_mode_calls: BTreeSet::new(),
        code_mode_nested_evidence: BTreeMap::new(),
        internal_artifact_origins: BTreeMap::new(),
        artifact_call_ids: BTreeMap::new(),
        model_visible_tool_result_token_budget: None,
        workspace_projection_cache: Arc::default(),
        ..Default::default()
    };
    let mut serialized = serde_json::to_value(&state).expect("serialize ledger state");
    let candidate = &serialized["candidates"]["call-1"];
    assert!(candidate.get("bounded_digest").is_some());
    assert!(candidate.get("bounded_model_output").is_none());
    serialized["provider_authoritative_outputs"] = serde_json::json!({"call-1": "legacy"});
    serialized["provider_baseline"] = serde_json::json!({"mode": "incremental"});
    serialized
        .as_object_mut()
        .expect("ledger object")
        .remove("untracked_consumption");

    let restored: ToolHistoryState =
        serde_json::from_value(serialized).expect("legacy fields should be ignored");
    assert!(restored.candidates.contains_key("call-1"));
    assert!(restored.untracked_consumption.is_empty());
    assert!(restored.recovered_ranges.is_empty());
}

#[tokio::test]
async fn recovered_source_does_not_displace_newer_evidence_after_persistence() {
    let temp = tempfile::tempdir().unwrap();
    let output = "exact source evidence ".repeat(500);
    let _budget = override_model_visible_tool_result_token_budget_for_test(
        approx_token_count(&output) + 1_000);
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for id in ["old-hot", "new-cold"] {
        let mut record = candidate(id, output.clone());
        let canonical = CanonicalToolResult::text(output.clone());
        let artifact = create_canonical_output_artifact(temp.path(), "thread", &canonical).await;
        record.artifact_id = artifact.artifact_id().unwrap();
        record.artifact_bytes = canonical.exact_bytes;
        record.artifact_sha256 = canonical.sha256;
        record.consumed_by_generation = Some(ModelGenerationId { turn_id: "turn".into(), ordinal: 1 });
        state.register(record);
        items.extend([function_call(id), text_output(id, output.clone())]);
    }
    let items: Arc<[ResponseItem]> = items.into();
    let baseline = state.project(Arc::clone(&items));
    assert!(baseline.items.iter().filter_map(textual_output_identity)
        .any(|(id, text)| id == "new-cold" && text == output));
    assert!(!ToolHistoryMutation::MarkArtifactRecovered { artifact_id: "unknown".into() }.apply(&mut state));
    persist_tool_history_state(temp.path(), "thread", &state).await.unwrap();
    let mutation = ToolHistoryMutation::MarkArtifactRecovered {
        artifact_id: state.candidates["old-hot"].artifact_id.clone(),
    };
    assert!(mutation.apply(&mut state));
    assert!(!mutation.apply(&mut state), "repeated recovery must not grow the ledger");
    persist_tool_history_mutations(temp.path(), "thread", "writer", &[(1, mutation)]).await.unwrap();
    let restored = expect_loaded_tool_history(load_tool_history_state(temp.path(), "thread").await);
    let projection = restored.project(items);
    let outputs = projection.items.iter().filter_map(textual_output_identity).collect::<Vec<_>>();
    assert!(!outputs.iter().any(|(id, text)| *id == "old-hot" && *text == output));
    assert!(outputs.iter().any(|(id, text)| *id == "new-cold" && *text == output));
    assert!(restored.reuse_priority("new-cold", 0) < restored.reuse_priority("old-hot", 5));
    assert!(restored.reuse_priority("new-unread", 2) < restored.reuse_priority("old-hot", 5));
    persist_tool_history_state(temp.path(), "thread", &restored).await.unwrap();
    let restored = expect_loaded_tool_history(load_tool_history_state(temp.path(), "thread").await);
    assert!(restored.recovered_call_ids.contains("old-hot"));
    let mut retired = restored;
    retired.retain_for_history(&[]);
    assert!(retired.recovered_call_ids.is_empty());
}

#[test]
fn recovered_evidence_does_not_promote_its_mixed_purpose_carrier() {
    let mut state = ToolHistoryState::default();
    let output = text_output("nested", "exact file evidence".into());
    state.register_workspace_evidence(WorkspaceEvidenceObservation::from_response_item(
        None, &output, BTreeSet::from([SourceDependencyV1::new(Path::new("file.txt"), false)])
    ).unwrap());
    assert!(ToolHistoryMutation::RegisterCodeModeNestedEvidence {
        parent_call_id: "outer".into(), call_id: "nested".into(), output: "exact file evidence".into(),
    }.apply(&mut state));
    assert!(ToolHistoryMutation::RegisterArtifactOrigin {
        artifact_id: "file-artifact".into(), call_id: "nested".into(), bytes: 19,
        sha256: sha256(b"exact file evidence"),
    }.apply(&mut state));
    assert_eq!(state.reuse_priority("outer", 5), state.reuse_priority("cold", 5));
    assert!(ToolHistoryMutation::MarkArtifactRecovered {
        artifact_id: "file-artifact".into(),
    }.apply(&mut state));
    let restored: ToolHistoryState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    assert_eq!(restored.reuse_priority("outer", 5), restored.reuse_priority("cold", 5));
    assert!(restored.reuse_priority("unread", 2) < restored.reuse_priority("outer", 5));
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
fn compaction_pins_exact_inline_output_but_rejects_same_count_changed_identifiers() {
    let call_id = "inventory-render";
    let output =
        r#"{"count":2,"identifiers":["prompts/compact/incremental.md","prompts/review/exit.xml"]}"#;
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, output.to_string()));
    let pins = state
        .artifact_pin_payload_for_items(&[text_output(call_id, output.to_string())])
        .unwrap();
    assert!(pins.contains("artifact-1"));
    for altered in [
        output.replace(".xml", ".md"),
        output.replace("compact/", "compaction/"),
    ] {
        assert!(
            state
                .artifact_pin_payload_for_items(&[text_output(call_id, altered)])
                .is_none()
        );
    }
    assert!(
        state
            .artifact_pin_payload_for_items(&[text_output("different-call", output.to_string())])
            .is_none()
    );
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

#[test]
fn token_backfire_compaction_pins_artifact_without_model_copying_receipt() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    let call_id = "call-1";
    let output = bounded_output();
    let mut state = ToolHistoryState::default();
    state.register(candidate(call_id, output.clone()));
    let projection = state.project(Arc::from([
        function_call(call_id),
        text_output(call_id, output),
    ]));

    let pin_payload = state
        .artifact_pin_payload_for_items(&projection.items)
        .expect("the compaction projection must yield a deterministic recovery sidecar");
    let pins: serde_json::Value = serde_json::from_str(&pin_payload).expect("valid pin payload");
    let artifact = &pins["artifacts"][0];
    assert_eq!(
        artifact["artifact_id"],
        state.candidates[call_id].artifact_id
    );
    let standalone = state.candidates[call_id]
        .artifact_pin_value()
        .expect("standalone artifact pin");
    assert_eq!(standalone["retrieval"]["tool"], "read_tool_output");
    assert!(artifact.get("retrieval").is_none());
    for instruction in [
        pins["instruction"].as_str().expect("sidecar instruction"),
        standalone["retrieval"]["instruction"]
            .as_str()
            .expect("pin instruction"),
    ] {
        for selector in ["search", "lines", "bytes", "section", "json_pointer"] {
            assert!(
                instruction.contains(selector),
                "missing selector guidance: {selector}"
            );
        }
        assert!(instruction.contains("continuation or child_selectors"));
    }
    state.retain_for_history(&[text_output("compaction-summary", pin_payload)]);

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
            journal_sequences: BTreeMap::new(),
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
        } if u64::from(found) == u64::from(LEDGER_VERSION.saturating_add(1))
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
async fn untracked_only_checkpoint_preserves_exposure_on_resume() {
    let temp = tempfile::tempdir().expect("tempdir");
    let thread_id = "untracked-only-thread";
    let mut state = ToolHistoryState::default();
    let generation = ModelGenerationId {
        turn_id: "investigation".into(),
        ordinal: 1,
    };
    let history = vec![
        function_call("observed-output"),
        text_output("observed-output", "completed source read".into()),
    ];
    assert!(state.mark_consumed(&history, generation.clone()));
    assert!(state.candidates.is_empty());
    assert!(!journal_path(temp.path(), thread_id).exists());

    persist_tool_history_state(temp.path(), thread_id, &state)
        .await
        .expect("persist exposure without an artifact or prior journal");
    let restored =
        expect_loaded_tool_history(load_tool_history_state(temp.path(), thread_id).await);
    assert_eq!(
        restored.untracked_consumption["observed-output"],
        generation
    );
    assert!(restored.output_was_consumed("observed-output"));
    assert!(!restored.output_was_consumed("unseen-output"));
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
    for (has_complete_prefix, incomplete_bytes) in [
        (true, 0),
        (true, 1),
        (true, 10),
        (true, 8_192),
        (true, 24_577),
        (false, 1),
        (false, 24_577),
    ] {
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
        let path = journal_path(temp.path(), thread_id);
        let committed_prefix = if has_complete_prefix {
            std::fs::read(&path).expect("read committed prefix")
        } else {
            std::fs::write(&path, []).expect("leave no complete prefix");
            Vec::new()
        };
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open journal tail")
            .write_all(&vec![b'x'; incomplete_bytes])
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

        let bytes = std::fs::read(&path).expect("read repaired journal");
        assert!(
            bytes.starts_with(&committed_prefix),
            "committed bytes must be preserved"
        );
        assert_eq!(bytes.last(), Some(&b'\n'));
        let records = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty());
        assert_eq!(records.count(), if has_complete_prefix { 2 } else { 1 });
        let restored = expect_loaded_tool_history(
            load_tool_history_state_for_fork(temp.path(), thread_id).await,
        );
        let expected = if has_complete_prefix {
            BTreeSet::from(["first-call".to_string(), "second-call".to_string()])
        } else {
            BTreeSet::from(["second-call".to_string()])
        };
        assert_eq!(restored.non_workspace_code_mode_calls, expected);
    }
}

#[tokio::test]
async fn corrupt_journal_record_keeps_checkpoint_and_valid_prefix() {
    let temp = tempfile::tempdir().expect("tempdir");
    let thread_id = "corrupt-journal-thread";
    let mut checkpoint = ToolHistoryState::default();
    checkpoint.register_non_workspace_code_mode_call("checkpoint-call".to_string());
    persist_tool_history_state(temp.path(), thread_id, &checkpoint)
        .await
        .expect("persist checkpoint");
    let register = |call_id: &str| ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
        call_id: call_id.to_string(),
    };
    persist_tool_history_mutations(
        temp.path(),
        thread_id,
        "writer",
        &[(1, register("prefix-call")), (2, register("after-corruption"))],
    )
    .await
    .expect("persist journal");
    let path = journal_path(temp.path(), thread_id);
    let journal = std::fs::read(&path).expect("read journal");
    let first_end = journal.iter().position(|byte| *byte == b'\n').expect("first record") + 1;
    let mut corrupted = journal[..first_end].to_vec();
    corrupted.extend_from_slice(b"{\"version\":1,\"not\":\"a record\"}\n");
    corrupted.extend_from_slice(&journal[first_end..]);
    std::fs::write(&path, &corrupted).expect("corrupt a complete middle record");

    let outcome = load_tool_history_state(temp.path(), thread_id).await;
    let ToolHistoryLoadOutcome::RecoveredJournalPrefix { state, path: _, error } = outcome else {
        panic!("expected a recovered journal prefix, got {outcome:?}");
    };
    assert_eq!(
        state.non_workspace_code_mode_calls,
        BTreeSet::from(["checkpoint-call".to_string(), "prefix-call".to_string()])
    );
    assert!(error.contains("quarantined"), "{error}");
    assert!(!path.exists(), "the invalid journal is moved aside");
    // No startup/terminal persist: crash immediately after recovery, then
    // restart twice. Recovery itself must have made the prefix durable.
    for _ in 0..2 {
        assert_eq!(
            expect_loaded_tool_history(load_tool_history_state(temp.path(), thread_id).await)
                .non_workspace_code_mode_calls,
            state.non_workspace_code_mode_calls
        );
    }
    let checkpoint: ToolHistoryLedgerFile = serde_json::from_slice(
        &std::fs::read(ledger_path(temp.path(), thread_id)).unwrap(),
    ).unwrap();
    assert_eq!(checkpoint.journal_sequences.get("writer"), Some(&1));

    // Also model a crash after checkpoint commit but before journal retirement.
    std::fs::write(&path, &corrupted).unwrap();
    let (recovered, warning) = load_tool_history_state(temp.path(), thread_id)
        .await.into_state_and_warning();
    assert!(warning.is_some());
    assert_eq!(recovered.non_workspace_code_mode_calls, state.non_workspace_code_mode_calls);
    assert_eq!(
        expect_loaded_tool_history(load_tool_history_state(temp.path(), thread_id).await)
            .non_workspace_code_mode_calls,
        state.non_workspace_code_mode_calls
    );
}

#[tokio::test]
async fn unsupported_history_files_fence_cold_and_cached_writers() {
    for cached in [false, true] {
        for future_journal in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let thread_id = "unsupported-write-boundary";
            let writer = Arc::new(std::sync::Mutex::new(ToolHistoryJournalWriter::default()));
            let mut state = ToolHistoryState::default();
            state.register_non_workspace_code_mode_call("checkpoint".into());
            persist_tool_history_state_with_writer(temp.path(), thread_id, &state, Arc::clone(&writer))
                .await.unwrap();
            let mutations = [(1, ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
                call_id: "journal".into(),
            })];
            persist_tool_history_mutations_with_writer(
                temp.path(), thread_id, "writer", &mutations, Arc::clone(&writer),
            ).await.unwrap();
            let ledger = ledger_path(temp.path(), thread_id);
            let journal = journal_path(temp.path(), thread_id);
            // The new format deliberately cannot decode as the old state or
            // mutation schema, and its version does not fit in a u8.
            let future_path = if future_journal { &journal } else { &ledger };
            std::fs::write(future_path, b"{\"version\":512,\"future_format\":[1,2,3]}\n").unwrap();
            let ledger_before = std::fs::read(&ledger).unwrap();
            let journal_before = std::fs::read(&journal).unwrap();
            let outcome = load_tool_history_state(temp.path(), thread_id).await;
            assert!(matches!(&outcome, ToolHistoryLoadOutcome::UnsupportedVersion { found: 512, .. }));
            let (empty, warning) = outcome.into_state_and_warning();
            assert!(warning.is_some_and(|message| message.contains("unsupported version")));
            let writer = if cached { writer } else { Arc::default() };
            // Exercise the same empty initial checkpoint that startup attempts.
            assert!(persist_tool_history_state_with_writer(
                temp.path(), thread_id, &empty, Arc::clone(&writer),
            ).await.is_err());
            assert!(persist_tool_history_mutations_with_writer(
                temp.path(), thread_id, "writer", &mutations, writer,
            ).await.is_err());
            assert_eq!(std::fs::read(&ledger).unwrap(), ledger_before);
            assert_eq!(std::fs::read(&journal).unwrap(), journal_before);
            assert_eq!(std::fs::read_dir(ledger.parent().unwrap()).unwrap().count(), 2);
        }
    }
}

async fn write_recovery_boundary_fixture(home: &Path, thread_id: &str) -> Vec<u8> {
    persist_tool_history_mutations(home, thread_id, "writer", &[(
        1,
        ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall { call_id: "prefix".into() },
    )]).await.unwrap();
    let path = journal_path(home, thread_id);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{not-json}\n");
    std::fs::write(&path, &bytes).unwrap();
    bytes
}

async fn assert_recovery_boundary_survives_two_restarts(home: &Path, thread_id: &str) {
    for _ in 0..2 {
        let outcome = load_tool_history_state(home, thread_id).await;
        assert!(matches!(&outcome,
            ToolHistoryLoadOutcome::RecoveredJournalPrefix { .. } | ToolHistoryLoadOutcome::Loaded(_)
        ));
        let (state, _) = outcome.into_state_and_warning();
        assert_eq!(state.non_workspace_code_mode_calls, BTreeSet::from(["prefix".to_string()]));
    }
    assert!(!journal_path(home, thread_id).exists());
}

#[tokio::test]
async fn recovery_checkpoint_failure_keeps_source_until_successful_restart() {
    let temp = tempfile::tempdir().unwrap();
    let thread_id = "recovery-checkpoint-failure-boundary";
    let bytes = write_recovery_boundary_fixture(temp.path(), thread_id).await;
    let failure = fail_next_tool_history_persistence_for_test(thread_id);
    failure.release();
    let outcome = load_tool_history_state(temp.path(), thread_id).await;
    let ToolHistoryLoadOutcome::RecoveredJournalPrefix { state, error, .. } = outcome else {
        panic!("expected recoverable prefix");
    };
    assert!(error.contains("injected tool-history persistence failure"), "{error}");
    assert!(state.non_workspace_code_mode_calls.contains("prefix"));
    assert!(!ledger_path(temp.path(), thread_id).exists());
    assert_eq!(std::fs::read(journal_path(temp.path(), thread_id)).unwrap(), bytes);
    drop(failure);
    assert_recovery_boundary_survives_two_restarts(temp.path(), thread_id).await;
}

#[tokio::test]
async fn recovery_checkpoint_cancellation_keeps_source_until_successful_restart() {
    let temp = tempfile::tempdir().unwrap();
    let thread_id = "recovery-checkpoint-cancellation-boundary";
    let bytes = write_recovery_boundary_fixture(temp.path(), thread_id).await;
    let pause = pause_next_tool_history_persistence_for_test(thread_id);
    let home = temp.path().to_path_buf();
    let recovery = tokio::spawn(async move { load_tool_history_state(&home, thread_id).await });
    pause.wait_until_reached().await;
    recovery.abort();
    assert!(recovery.await.unwrap_err().is_cancelled());
    drop(pause);
    assert!(!ledger_path(temp.path(), thread_id).exists());
    assert_eq!(std::fs::read(journal_path(temp.path(), thread_id)).unwrap(), bytes);
    assert_recovery_boundary_survives_two_restarts(temp.path(), thread_id).await;
}

#[tokio::test]
async fn fork_remints_receipt_artifact_into_child_namespace() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
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
        journal_sequences: BTreeMap::new(),
        version: LEDGER_VERSION,
        state: state.clone(),
    })
    .expect("serialize owned compatibility envelope");
    let borrowed = serde_json::to_vec(&ToolHistoryLedgerRef {
        journal_sequences: &BTreeMap::new(),
        version: LEDGER_VERSION,
        state: &state,
    })
    .expect("serialize borrowed ledger envelope");

    assert_eq!(borrowed, owned);
}

#[test]
fn cargo_manifest_read_failures_invalidate_receipts_without_losing_selective_reuse() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    let temp = tempfile::tempdir().expect("workspace fixture");
    let workspace = temp.path().join("workspace");
    let app = workspace.join("app");
    let support = temp.path().join("support");
    let transitive = temp.path().join("transitive");
    let unrelated = temp.path().join("unrelated.rs");
    for root in [&workspace, &app, &support, &transitive] {
        std::fs::create_dir_all(root).expect("package directory");
    }
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"app\"]\nresolver = \"2\"\n[workspace.dependencies]\nrenamed = { package = \"support\", path = \"../support\" }\n",
    )
    .expect("workspace manifest");
    std::fs::write(
        app.join("Cargo.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n[target.'cfg(windows)'.build-dependencies]\nrenamed.workspace = true\n",
    )
    .expect("app manifest");
    std::fs::write(
        support.join("Cargo.toml"),
        "[package]\nname = \"support\"\nversion = \"0.1.0\"\n[dependencies]\ntransitive = { path = \"../transitive\" }\n",
    )
    .expect("support manifest");
    std::fs::write(
        transitive.join("Cargo.toml"),
        "[package]\nname = \"transitive\"\nversion = \"0.1.0\"\n",
    )
    .expect("transitive manifest");
    for (case, failed_manifest, missing) in [
        ("complete", None, false),
        ("workspace-wide", None, false),
        (
            "workspace-unreadable",
            Some(workspace.join("Cargo.toml")),
            false,
        ),
        (
            "workspace-missing",
            Some(workspace.join("Cargo.toml")),
            true,
        ),
        ("selected-unreadable", Some(app.join("Cargo.toml")), false),
        ("selected-malformed", Some(app.join("Cargo.toml")), false),
        ("duplicate-package", Some(support.join("Cargo.toml")), false),
        (
            "intermediate-unreadable",
            Some(support.join("Cargo.toml")),
            false,
        ),
    ] {
        let complete = case == "complete";
        let arguments = if case == "workspace-wide" {
            serde_json::json!({"workdir": workspace})
        } else {
            serde_json::json!({"package": "app", "workdir": workspace})
        };
        let payload = ToolPayload::Function {
            arguments: arguments.to_string(),
        };
        let original = failed_manifest.as_ref().map(|path| {
            let original = std::fs::read(path).expect("save manifest");
            if missing {
                std::fs::remove_file(path).expect("temporarily remove manifest");
            } else if case == "duplicate-package" {
                std::fs::write(path, "[package]\nname = 'app'\nversion = '0.2.0'\n")
                    .expect("ambiguous package identity");
            } else if case == "selected-malformed" {
                std::fs::write(
                    path,
                    "[package]\nname = 'app'\n[dependencies]\nrenamed = { workspace = true\n",
                )
                .expect("temporarily malformed manifest");
            } else {
                // Invalid UTF-8 makes the actual read_to_string fail on every platform.
                std::fs::write(path, [0xff]).expect("temporarily unreadable manifest");
            }
            original
        });
        let classification = classify_workspace_tool_call("cargo_test", &payload, &workspace);
        if let Some(path) = &failed_manifest {
            // The external command may succeed after the discovery-time failure clears.
            std::fs::write(path, original.expect("saved manifest")).expect("restore manifest");
        }
        assert!(classification.observes_workspace, "{case}");
        assert_eq!(classification.workspace_cwd, workspace, "{case}");
        if complete {
            assert!(
                classification
                    .source_dependencies
                    .contains(&SourceDependencyV1::new(&transitive, true,))
            );
            assert!(
                !classification
                    .source_dependencies
                    .contains(&SourceDependencyV1::new(temp.path(), true,))
            );
        }

        let bounded = bounded_output();
        let mut output = text_output(case, bounded.clone());
        let ResponseItem::FunctionCallOutput { output: body, .. } = &mut output else {
            unreachable!();
        };
        body.success = Some(true);
        let canonical: Arc<[ResponseItem]> = Arc::from([
            named_function_call_with_arguments(case, "cargo_test", arguments.clone()),
            output.clone(),
        ]);
        let mut tracked = candidate(case, bounded);
        tracked.tool_identity = "cargo_test".to_string();
        tracked.source_dependencies = classification.source_dependencies.clone();
        let known_dependencies = !tracked.source_dependencies.is_empty();
        let captured = workspace_identity("captured");
        let mut state = ToolHistoryState::default();
        state.register(tracked);
        state.register_workspace_evidence(
            WorkspaceEvidenceObservation::from_response_item_with_freshness(
                Some(captured.clone()),
                &output,
                classification.source_dependencies,
                true,
            )
            .expect("successful tool observation"),
        );
        assert!(state.mark_consumed(
            &canonical,
            ModelGenerationId {
                turn_id: case.to_string(),
                ordinal: 1
            },
        ));
        let initial =
            state.project_with_workspace_identity(Arc::clone(&canonical), Some(&captured));
        let (_, receipt_text) =
            textual_output_identity(&initial.items[1]).expect("initial receipt");
        if known_dependencies {
            let receipt: ToolHistoryReceiptV2 =
                serde_json::from_str(receipt_text).expect("valid scoped receipt");
            assert_eq!(receipt.call_id, case);
            assert_eq!(receipt.artifact_id, "artifact-1");
            assert_eq!(receipt.bytes, 96_000);
        } else {
            let notice: serde_json::Value = serde_json::from_str(receipt_text).unwrap();
            assert_eq!(notice["workspace_evidence_freshness"], "unknown");
            assert_eq!(notice["historical_authenticity"], "authenticated");
            assert!(notice["historical_digest"].as_str().unwrap().contains("bounded model-visible tool output"));
        }

        for (path, must_be_stale) in [(&unrelated, !complete), (&transitive.join("lib.rs"), true)] {
            let changed = workspace_identity("changed");
            state.invalidate_source_dependencies(
                Some(&BTreeSet::from([path.clone()])),
                Some(&changed),
            );
            let projected =
                state.project_with_workspace_identity(Arc::clone(&canonical), Some(&changed));
            if must_be_stale {
                let (_, text) = textual_output_identity(&projected.items[1]).expect("stale output");
                let mut value: serde_json::Value =
                    serde_json::from_str(text).expect("stale evidence JSON");
                let digest = value
                    .as_object_mut()
                    .unwrap()
                    .remove("historical_digest")
                    .expect("authenticated historical output");
                let digest = digest.as_str().unwrap();
                assert!(digest.contains("bounded model-visible tool output"));
                assert!(approx_token_count(digest) <= RECEIPT_DIGEST_TARGET_TOKENS);
                assert_eq!(value["qualification"], "Currentness unknown; no dependency change established");
                assert_eq!(value["historical_authenticity"], "authenticated");
                assert_eq!(value["workspace_evidence_freshness"], "unknown");
                assert_eq!(value["source_scope"]["dependency_scope"], if known_dependencies { "recorded" } else { "unknown" });
                assert!(value["source_scope"]["dependencies"].as_array().is_some_and(|dependencies|
                    dependencies.is_empty() != known_dependencies
                        && dependencies.iter().all(|dependency| dependency["freshness"] == "unknown")));
                for key in ["qualification", "historical_authenticity", "workspace_evidence_freshness", "source_scope"] {
                    value.as_object_mut().unwrap().remove(key);
                }
                assert_eq!(
                    value,
                    serde_json::json!({
                        "call_id": case,
                        "rerun": { "instruction": "This notice is not a request to rerun tests or builds. Revalidate only if current evidence is essential to the task, using the cheapest scoped read or check with supported arguments; otherwise report the affected claim as unverified. Do not add recovery-only arguments. Do not replay writes or restart a live command; continue its existing session. Reading a retained artifact recovers historical bytes, not current workspace evidence." },
                        "reason": "source dependencies were invalidated after capture; this does not establish which dependency changed; current workspace freshness is unverified",
                        "stale_workspace_evidence": true,
                        "reason_code": "source_dependencies_invalidated",
                        "valid_for_current_workspace": false,
                        "observed_revision": if complete { &changed } else { &captured },
                        "current_revision": changed,
                        "if_rerun_unavailable": "Report the affected claim as unverified; this result does not validate the current workspace.",
                    }),
                    "{case}"
                );
            } else {
                assert_eq!(
                    projected.items, initial.items,
                    "unrelated edit must preserve exact receipt"
                );
            }
        }
    }
}

#[test]
fn tool_history_admission_recovers_legacy_non_text_cost_from_response_content() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(10_000);
    let image = FunctionCallOutputContentItem::InputImage {
        image_url: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==".to_string(),
        detail: None,
    };
    let images = vec![image; 300];
    let non_text_tokens = approx_token_count(&serde_json::to_string(&images).unwrap());
    assert!(non_text_tokens > model_visible_tool_result_token_budget());
    for legacy_missing_cost in [false, true] {
        let mut state = ToolHistoryState::default();
        let mut canonical = Vec::new();
        for call_id in ["older-image", "newest-image"] {
            let text = format!("image result for {call_id}");
            let mut registered = candidate(call_id, text.clone());
            registered.artifact_id = format!("artifact-{call_id}");
            registered.preserved_non_text_tokens = Some(non_text_tokens as u64);
            let mut serialized = serde_json::to_value(registered).unwrap();
            if legacy_missing_cost {
                serialized
                    .as_object_mut()
                    .unwrap()
                    .remove("preserved_non_text_tokens");
            }
            let restored: ToolHistoryCandidate = serde_json::from_value(serialized).unwrap();
            assert_eq!(
                restored.preserved_non_text_tokens,
                (!legacy_missing_cost).then_some(non_text_tokens as u64)
            );
            state.register(restored);
            canonical.push(function_call(call_id));
            let mut content = vec![FunctionCallOutputContentItem::InputText { text }];
            content.extend(images.clone());
            canonical.push(ResponseItem::FunctionCallOutput {
                id: None,
                call_id: call_id.to_string(),
                output: FunctionCallOutputPayload::from_content_items(content),
                internal_chat_message_metadata_passthrough: None,
            });
        }
        let expected: Arc<[ResponseItem]> = Arc::from(canonical[2..].to_vec());
        let projection = state.project(Arc::from(canonical));
        for items in [&projection.items, &projection.unreplaced_items] {
            assert_eq!(
                &items[..expected.len()],
                expected.as_ref(),
                "legacy_missing_cost={legacy_missing_cost}"
            );
            assert_eq!(items.len(), expected.len() + 1);
            assert!(
                matches!(&items[expected.len()], ResponseItem::Message { role, content, .. }
                if role == "developer" && format!("{content:?}").contains("1 unread outcomes"))
            );
        }
        assert!(projection.substitutions.is_empty());
    }

    // Persisted zero is a known text-only cost; only legacy omission needs a scan.
    // Both remain eligible for normal raw reuse after restoring the ledger.
    for legacy_missing_cost in [false, true] {
        let call_id = "text-only";
        let text = "exact text-only result".to_string();
        let mut serialized = serde_json::to_value(candidate(call_id, text.clone())).unwrap();
        assert_eq!(serialized["preserved_non_text_tokens"], 0);
        if legacy_missing_cost {
            serialized
                .as_object_mut()
                .unwrap()
                .remove("preserved_non_text_tokens");
        }
        let restored: ToolHistoryCandidate = serde_json::from_value(serialized).unwrap();
        assert_eq!(
            restored.preserved_non_text_tokens,
            (!legacy_missing_cost).then_some(0)
        );
        let mut state = ToolHistoryState::default();
        state.register(restored);
        let canonical: Arc<[ResponseItem]> =
            Arc::from([function_call(call_id), text_output(call_id, text)]);
        state.register_non_workspace_code_mode_call(call_id.to_string());
        let projection = state.project_with_workspace_identity(Arc::clone(&canonical), None);
        assert_eq!(projection.items, canonical);
        assert_eq!(projection.unreplaced_items, canonical);
        assert!(projection.substitutions.is_empty());
    }
}

#[test]
fn cargo_dependencies_resolve_inherited_renamed_external_paths_and_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    for path in [
        "workspace/crates/app",
        "external",
        "workspace/unrelated/hidden",
    ] {
        std::fs::create_dir_all(temp.path().join(path)).unwrap();
    }
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = ['crates/*']\n[workspace.dependencies]\nrenamed = { package = 'support', path = '../external' }\n").unwrap();
    let app = root.join("crates/app/Cargo.toml");
    std::fs::write(&app, "[package]\nname = 'app'\nversion = '0.1.0'\n[target.'cfg(windows)'.build-dependencies]\nrenamed.workspace = true\n").unwrap();
    std::fs::write(
        temp.path().join("external/Cargo.toml"),
        "[package]\nname = 'support'\nversion = '0.1.0'\n",
    )
    .unwrap();
    std::fs::write(
        root.join("unrelated/hidden/Cargo.toml"),
        "[package]\nname = 'hidden'\nversion = '0.1.0'\n",
    )
    .unwrap();
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({"package": "app"}).to_string(),
    };
    let dependencies = source_dependencies_for_tool_call("cargo_test", &payload, &root);
    assert!(dependencies.contains(&SourceDependencyV1::new(
        &temp.path().join("external"),
        true
    )));
    assert!(dependencies.contains(&SourceDependencyV1::new(&root.join("crates/app"), true)));
    let mut reads = Vec::new();
    let graph = cargo_package_index_with_manifest_reader(
        &root,
        Some(std::fs::read_to_string(root.join("Cargo.toml")).unwrap()),
        |path| {
            reads.push(path.to_path_buf());
            std::fs::read_to_string(path).ok()
        },
    );
    assert_eq!(reads.len(), 2);
    assert!(!graph.packages.contains_key("hidden"));
    std::fs::write(&app, "[package]\nname = 'app'\nversion = '0.1.0'\n[dependencies]\nrenamed = { workspace = true\n").unwrap();
    assert!(source_dependencies_for_tool_call("cargo_test", &payload, &root).is_empty());
}

#[tokio::test]
async fn checkpoint_ignores_old_journal_after_crash_but_replays_new_records() {
    let temp = tempfile::tempdir().unwrap();
    let thread_id = "checkpoint-crash";
    for writer in ["old-writer", "new-writer"] {
        persist_tool_history_mutations(
            temp.path(),
            thread_id,
            writer,
            &[(
                1,
                ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
                    call_id: format!("obsolete-{writer}"),
                },
            )],
        )
        .await
        .unwrap();
    }
    let old_journal = std::fs::read(journal_path(temp.path(), thread_id)).unwrap();
    persist_tool_history_state(temp.path(), thread_id, &ToolHistoryState::default())
        .await
        .unwrap();
    // Crash between the durable checkpoint rename and journal unlink.
    std::fs::write(journal_path(temp.path(), thread_id), old_journal).unwrap();
    persist_tool_history_mutations(
        temp.path(),
        thread_id,
        "new-writer",
        &[(
            2,
            ToolHistoryMutation::RegisterNonWorkspaceCodeModeCall {
                call_id: "current".to_string(),
            },
        )],
    )
    .await
    .unwrap();
    let restored =
        expect_loaded_tool_history(load_tool_history_state_for_fork(temp.path(), thread_id).await);
    assert_eq!(
        restored.non_workspace_code_mode_calls,
        BTreeSet::from(["current".to_string()])
    );
}
#[test]
fn workspace_dependencies_preserve_powershell_parser_host() {
    let script = "Get-Content 'src/foo.rs'";
    for executable in [
        "pwsh.exe",
        "powershell.exe",
        "C:/Program Files/PowerShell/7/pwsh.exe",
    ] {
        let arguments = serde_json::json!({"cmd": script, "shell": executable});
        let (command, shell_type) = dependency_search_command(&arguments).expect("script command");
        assert_eq!(command, vec![executable, "-Command", script]);
        assert_eq!(shell_type, Some(crate::shell::ShellType::PowerShell));
    }
    let shell = crate::shell::default_user_shell();
    if shell.shell_type == crate::shell::ShellType::PowerShell {
        let (command, _) = dependency_search_command(&serde_json::json!({"cmd": script}))
            .expect("default shell command");
        assert_eq!(command[0], shell.shell_path.to_string_lossy());
    }
}

#[test]
fn aggregate_budget_preserves_freshness_warning_when_pinning_historical_output() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(700);
    let mut state = ToolHistoryState::default();
    state.register(candidate("stale", "old evidence ".repeat(2000)));
    let notice = serde_json::json!({
        "stale_workspace_evidence": true,
        "valid_for_current_workspace": false,
        "reason_code": "source_dependencies_invalidated",
        "rerun": { "instruction": "Revalidate the original source, not this immutable artifact." },
        "historical_output": "old evidence ".repeat(2000),
    });
    let mut items = ProjectedResponseItems::Owned(vec![text_output("stale", notice.to_string())]);
    assert_eq!(state.enforce_tool_result_budget(&mut items).count, 0);
    let (_, text) = canonical_textual_output_identity(&items[0]).unwrap();
    let compact: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(compact["kind"], "tool_history_artifact_pin");
    assert_eq!(compact["stale_workspace_evidence"], true);
    assert_eq!(compact["valid_for_current_workspace"], false);
    assert_eq!(compact["reason_code"], notice["reason_code"]);
    assert_eq!(compact["rerun"], notice["rerun"]);
    assert!(approx_token_count(&text) <= 700);
}

#[test]
fn aggregate_pressure_keeps_fitting_unread_diagnostics_verbatim() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(700);
    let detail = format!(
        "write failed: permission denied\n{}",
        "details ".repeat(150)
    );
    let mut failure = text_output("failure", detail.clone());
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut failure {
        output.success = Some(false);
    }
    let mut items =
        ProjectedResponseItems::Owned(vec![failure, text_output("large", "output ".repeat(2000))]);
    let state = ToolHistoryState::default();
    assert_eq!(state.enforce_tool_result_budget(&mut items).count, 0);
    let (_, actual) = items
        .iter()
        .filter_map(canonical_textual_output_identity)
        .find(|(call_id, _)| *call_id == "failure")
        .unwrap();
    assert_eq!(actual, detail);
    let (_, compact) = items
        .iter()
        .filter_map(canonical_textual_output_identity)
        .find(|(call_id, _)| *call_id == "large")
        .unwrap();
    let compact: serde_json::Value = serde_json::from_str(&compact).unwrap();
    assert_eq!(compact["output_omitted"], true);
    assert!(
        items
            .iter()
            .filter_map(canonical_textual_output_identity)
            .map(|(_, text)| approx_token_count(&text))
            .sum::<usize>()
            <= 700
    );
}

#[test]
fn observed_untracked_output_does_not_displace_newer_source_evidence() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(700);
    let mut state = ToolHistoryState::default();
    let old = "old detail ".repeat(220);
    let source = format!("{}\nrequired final condition", "source ".repeat(260));
    state.register(candidate("source", source.clone()));
    let history: Arc<[ResponseItem]> = Arc::from([
        function_call("old"),
        text_output("old", old),
        function_call("source"),
        text_output("source", source.clone()),
    ]);
    let generation = ModelGenerationId {
        turn_id: "investigation".into(),
        ordinal: 2,
    };
    // Both results were actually delivered, but only the source has an artifact.
    assert_eq!(
        state.mark_consumed_with_delta(&history, generation.clone()),
        BTreeSet::from(["old".into(), "source".into()])
    );
    assert!(
        state
            .mark_consumed_with_delta(&history, generation)
            .is_empty()
    );
    let projection = state.project(Arc::clone(&history));
    for items in [&projection.items, &projection.unreplaced_items] {
        let (_, actual) = items
            .iter()
            .filter_map(canonical_textual_output_identity)
            .find(|(id, _)| *id == "source")
            .expect("recent source remains available after its first exposure");
        assert_eq!(actual, source);
        assert!(
            items
                .iter()
                .filter_map(canonical_textual_output_identity)
                .map(|(_, text)| approx_token_count(&text))
                .sum::<usize>()
                <= 700
        );
    }
    assert_eq!(
        canonical_textual_output_identity(&history[3]).unwrap().1,
        source
    );
}

#[tokio::test]
async fn untracked_exposure_survives_journal_checkpoint_and_fork() {
    let temp = tempfile::tempdir().unwrap();
    let generation = ModelGenerationId {
        turn_id: "turn".into(),
        ordinal: 1,
    };
    let mut state = ToolHistoryState::default();
    let history = vec![
        function_call("seen"),
        text_output("seen", "completed read".into()),
    ];
    let ids = state.mark_consumed_with_delta(&history, generation.clone());
    assert_eq!(ids, BTreeSet::from(["seen".into()]));
    assert!(!state.output_was_consumed("undelivered"));
    persist_tool_history_mutations(
        temp.path(),
        "parent",
        "writer",
        &[(
            1,
            ToolHistoryMutation::MarkConsumed {
                call_ids: ids,
                generation: generation.clone(),
                exposed_representations: BTreeMap::new(),
            },
        )],
    )
    .await
    .unwrap();
    let restored =
        expect_loaded_tool_history(load_tool_history_state_for_fork(temp.path(), "parent").await);
    assert_eq!(
        restored.untracked_consumption.get("seen"),
        Some(&generation)
    );
    persist_tool_history_state(temp.path(), "parent", &restored)
        .await
        .unwrap();
    let restored =
        expect_loaded_tool_history(load_tool_history_state_for_fork(temp.path(), "parent").await);
    let (mut forked, dropped) =
        remint_tool_history_state_for_fork(temp.path(), "parent", "child", restored).await;
    assert_eq!(dropped, 0);
    assert!(forked.output_was_consumed("seen"));
    forked.retain_for_history(&history);
    assert!(forked.output_was_consumed("seen"));
    forked.retain_for_history(&[]);
    assert!(!forked.output_was_consumed("seen"));
    let legacy: ToolHistoryState = serde_json::from_str("{}").unwrap();
    assert!(!legacy.output_was_consumed("seen"));
}

#[test]
fn untracked_exposure_preserves_fresh_failures_and_late_artifact_registration() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(700);
    let mut state = ToolHistoryState::default();
    let generation = ModelGenerationId {
        turn_id: "turn".into(),
        ordinal: 1,
    };
    let old = text_output("seen", "old detail ".repeat(220));
    assert!(state.mark_consumed(&[old.clone()], generation.clone()));
    let detail = format!(
        "{}\nwrite failed: permission denied",
        "diagnostic ".repeat(160)
    );
    let mut failure = text_output("failure", detail);
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut failure {
        output.success = Some(false);
    }
    let projection = state.project(Arc::from([old, failure.clone()]));
    for items in [&projection.items, &projection.unreplaced_items] {
        assert!(
            items.contains(&failure),
            "an undelivered failure retains its full diagnostics"
        );
    }
    assert!(!state.output_was_consumed("failure"));
    state.register(candidate("seen", "old detail ".repeat(220)));
    assert_eq!(
        state.candidates["seen"].consumed_by_generation,
        Some(generation)
    );
    assert!(!state.untracked_consumption.contains_key("seen"));
}

#[test]
fn budget_receipts_distinguish_observed_and_unread_untracked_outputs() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(700);
    let mut state = ToolHistoryState::default();
    let old = text_output("seen", "previous observation ".repeat(2_000));
    state.mark_consumed(
        &[old.clone()],
        ModelGenerationId {
            turn_id: "turn".into(),
            ordinal: 1,
        },
    );
    let unread = text_output("unread", "fresh observation ".repeat(2_000));
    let projection = state.project(Arc::from([old, unread]));
    for items in [&projection.items, &projection.unreplaced_items] {
        let receipts = items
            .iter()
            .filter_map(canonical_textual_output_identity)
            .map(|(id, text)| {
                (
                    id,
                    serde_json::from_str::<serde_json::Value>(&text).unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(receipts["seen"]["kind"], "observed_tool_outcome");
        assert_eq!(receipts["unread"]["kind"], "unconsumed_tool_outcome");
        assert_eq!(receipts["seen"]["output_omitted"], true);
        assert_eq!(receipts["unread"]["output_omitted"], true);
    }
}

#[test]
fn projected_prompt_preserves_fresh_and_recovered_detail_before_consumed_history() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(700);
    for tool in ["read_file", "read_tool_output"] {
        let mut state = ToolHistoryState::default();
        let old_text = "old validation evidence ".repeat(2000);
        let mut old = candidate("old", old_text.clone());
        old.consumed_by_generation = Some(ModelGenerationId {
            turn_id: "turn".to_string(),
            ordinal: 0,
        });
        state.register(old);
        state.register_non_workspace_code_mode_call("old".to_string());
        let fresh = format!(
            "{}\nDECISIVE: this entire late diagnostic must reach the model",
            "detail ".repeat(150)
        );
        if tool == "read_file" {
            state.register(candidate("fresh", fresh.clone()));
        }
        state.register_non_workspace_code_mode_call("fresh".to_string());
        let canonical: Arc<[ResponseItem]> = Arc::from([
            function_call("old"),
            text_output("old", old_text),
            named_function_call_with_arguments("fresh", tool, serde_json::json!({})),
            text_output("fresh", fresh.clone()),
        ]);
        let projection = state.project(canonical);
        for items in [&projection.items, &projection.unreplaced_items] {
            let (_, actual) = items
                .iter()
                .filter_map(canonical_textual_output_identity)
                .find(|(call_id, _)| *call_id == "fresh")
                .expect("fresh result survives prompt projection");
            assert_eq!(
                actual, fresh,
                "{tool}: preserve body, not only outcome or handle"
            );
        }
    }
}

#[test]
fn unread_failure_and_live_handle_survive_aggregate_pressure() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(700);
    let state = ToolHistoryState::default();
    let mut failure = text_output(
        "failed-write",
        format!(
            "write failed: permission denied\n{}",
            "details ".repeat(2000)
        ),
    );
    if let ResponseItem::FunctionCallOutput { output, .. } = &mut failure {
        output.success = Some(false);
    }
    let live = text_output(
        "running",
        format!(
            "Process running with session ID 12345\n{}",
            "output ".repeat(2000)
        ),
    );
    let mut items = ProjectedResponseItems::Owned(vec![failure, live]);
    let drops = state.enforce_tool_result_budget(&mut items);
    assert_eq!(drops.count, 0);
    let combined = items
        .iter()
        .filter_map(canonical_textual_output_identity)
        .map(|(_, text)| text.into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(combined.contains("permission denied"));
    assert!(combined.contains("12345"));
    assert!(combined.contains("\"successful\":false"));
    assert!(
        items
            .iter()
            .filter_map(canonical_textual_output_identity)
            .map(|(_, output)| approx_token_count(&output))
            .sum::<usize>()
            <= 700
    );
}

#[test]
fn observed_failure_detail_does_not_evict_an_unread_live_handle() {
    let old_output = "previously observed failure detail ".repeat(100);
    let _budget =
        override_model_visible_tool_result_token_budget_for_test(approx_token_count(&old_output));
    let mut old = candidate("old-failure", old_output.clone());
    old.successful = false;
    old.consumed_by_generation = Some(ModelGenerationId {
        turn_id: "observed-turn".to_string(),
        ordinal: 1,
    });
    let mut state = ToolHistoryState::default();
    state.candidates.insert(old.call_id.clone(), old);
    let mut items = ProjectedResponseItems::Owned(vec![
        text_output("old-failure", old_output),
        text_output(
            "new-live",
            format!(
                "Process running with session ID 12345\n{}",
                "output ".repeat(2000),
            ),
        ),
    ]);
    let drops = state.enforce_tool_result_budget(&mut items);
    assert_eq!(drops.count, 0);
    let old = items
        .iter()
        .find(|item| item_call_id(item) == Some("old-failure"))
        .expect("the older result can still fit as an exact recovery handle");
    let (_, old_text) = canonical_textual_output_identity(old).unwrap();
    let old_pin: serde_json::Value = serde_json::from_str(&old_text).unwrap();
    assert_eq!(old_pin["artifact_id"], "artifact-1");
    assert_eq!(old_pin["successful"], false);
    let live = items
        .iter()
        .find(|item| item_call_id(item) == Some("new-live"))
        .expect("new continuation must survive previously observed detail");
    assert!(
        canonical_textual_output_identity(live)
            .unwrap()
            .1
            .contains("12345")
    );
    assert_eq!(
        items.len(),
        2,
        "both the live handle and the older recovery handle fit"
    );
    assert!(
        items
            .iter()
            .filter_map(canonical_textual_output_identity)
            .map(|(_, text)| approx_token_count(&text))
            .sum::<usize>()
            <= model_visible_tool_result_token_budget()
    );
}

#[test]
fn impossible_unread_receipt_budget_reports_unresolved_overflow() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(1);
    let mut state = ToolHistoryState::default();
    state.register(candidate("tracked", "tracked output".into()));
    let mut items = ProjectedResponseItems::Owned(vec![
        function_call("tracked"),
        text_output("tracked", "tracked output".into()),
        function_call("unread"),
        text_output("unread", "operation failed".into()),
    ]);
    let dropped = state.enforce_tool_result_budget(&mut items);
    assert_eq!(dropped.count, 2);
    assert!(items.iter().all(|item| item_call_id(item).is_none()));
    let ResponseItem::Message { content, .. } = &items[0] else {
        panic!("expected overflow notice");
    };
    let [codex_protocol::models::ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected one text notice");
    };
    assert!(text.contains("2 unread outcomes"));
    // Pairs are removed whole, so recovery depends on the notice naming them.
    let manifest = text
        .split_once("Manifest page 1/1, offset 0: ")
        .and_then(|(_, rest)| rest.split_once(". Recover each"))
        .map(|(manifest, _)| manifest)
        .expect("overflow manifest");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(manifest).unwrap(),
        serde_json::json!([
            {"call_id": "tracked", "artifact_id": "artifact-1"},
            {"call_id": "unread"},
        ])
    );
}

#[tokio::test]
async fn audit_equal_git_identity_does_not_override_dependency_watcher() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("ignored-input.txt");
    std::fs::write(&source, "before").unwrap();
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let proof = cache
        .begin_source_path_change_observation(root.path(), &source, false)
        .await
        .unwrap();
    let mut revision = workspace_identity("unchanged-git-visible-state");
    revision.repository_root = Some(root.path().to_string_lossy().into_owned());
    let output = text_output("ignored-read", "before".to_owned());
    let canonical: Arc<[ResponseItem]> = Arc::from([function_call("ignored-read"), output.clone()]);
    let mut state = ToolHistoryState::default();
    state.register_workspace_evidence(
        WorkspaceEvidenceObservation::from_response_item(
            Some(revision.clone()),
            &output,
            BTreeSet::from([SourceDependencyV1::new(&source, false)]),
        )
        .unwrap()
        .with_source_path_observations(vec![proof]),
    );
    assert_eq!(
        state
            .project_with_workspace_cache(Arc::clone(&canonical), Some(&revision), &cache)
            .items,
        canonical
    );
    std::fs::write(&source, "after!").unwrap();
    cache
        .note_host_workspace_mutation_paths(root.path(), &["ignored-input.txt".to_owned()])
        .await;
    let projected = state.project_with_workspace_cache(canonical, Some(&revision), &cache);
    let (_, text) = textual_output_identity(&projected.items[1]).unwrap();
    assert!(text.contains("stale_workspace_evidence"));
    assert!(text.contains("workspace_freshness_unverified"));
}

#[test]
fn unread_structured_admission_overflow_is_explicit_in_both_projections() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(1);
    let projection =
        ToolHistoryState::default().project(Arc::from(tool_search_pair("unread-search", 1000)));
    for items in [&projection.items, &projection.unreplaced_items] {
        assert!(
            items
                .iter()
                .all(|item| item_call_id(item) != Some("unread-search"))
        );
        assert!(items.iter().any(|item| matches!(item,
            ResponseItem::Message { content, .. } if format!("{content:?}").contains("1 unread outcomes"))));
    }
}

#[test]
fn explicit_read_and_search_dependencies_are_not_lost_after_eight_paths() {
    let cwd = Path::new("/repo");
    let paths = (0..12)
        .map(|i| format!("src/file{i}.rs"))
        .collect::<Vec<_>>();
    let expected = paths
        .iter()
        .map(|path| SourceDependencyV1::new(&cwd.join(path), false))
        .collect::<BTreeSet<_>>();
    for program in ["cat", "get-content"] {
        let mut command = vec![program.to_string()];
        command.extend(paths.clone());
        assert_eq!(
            dependencies_for_read_command(program, &command, cwd),
            expected
        );
    }
    assert_eq!(dependencies_for_search_scopes(paths, cwd), expected);
}

#[test]
fn internal_artifact_provenance_and_protection_survive_resume_and_history_retention() {
    let mut state = ToolHistoryState::default();
    let sha = sha256(b"source snapshot");
    ToolHistoryMutation::RegisterArtifactOrigin {
        artifact_id: "snapshot-id".into(),
        call_id: "inventory-producer".into(),
        bytes: 15,
        sha256: sha.clone(),
    }
    .apply(&mut state);
    let mut resumed: ToolHistoryState =
        serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    resumed.refresh_derived_and_indexes();
    resumed.rebuild_artifact_mapping("snapshot-id");
    let items = vec![
        named_function_call_with_arguments(
            "read",
            "read_tool_output",
            serde_json::json!({"artifact_id":"snapshot-id"}),
        ),
        text_output("read", "exact snapshot".into()),
    ];
    resumed.retain_for_history(&items);
    assert_eq!(
        resumed
            .artifact_call_ids
            .get("snapshot-id")
            .map(String::as_str),
        Some("inventory-producer")
    );
    assert_eq!(
        resumed.artifact_references().get("snapshot-id"),
        Some(&(15, sha))
    );
    assert!(
        resumed
            .artifact_pin_payload_for_items(&items)
            .unwrap()
            .contains("snapshot-id")
    );
    resumed.retain_for_history(&[]);
    assert!(resumed.is_persisted_empty());
}
#[test]
fn recovery_priority_and_exact_selectors_survive_compaction_metadata() {
    let mut state = ToolHistoryState::default();
    let record = candidate("origin", bounded_output());
    let artifact_id = record.artifact_id.clone();
    state.register(record);
    let selector = serde_json::json!({"kind": "bytes", "start": 100, "end": 200});
    let mutation = ToolHistoryMutation::RecordArtifactRecovery {
        artifact_id: artifact_id.clone(), recovery_call_id: "recovery".into(),
        selectors: vec![selector.clone()],
    };
    assert!(mutation.apply(&mut state));
    assert!(!mutation.apply(&mut state));
    let mut restored: ToolHistoryState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    restored.refresh_derived_and_indexes();
    assert!(restored.reuse_priority("recovery", 2) < restored.reuse_priority("cold", 2));
    let output = text_output("recovery", serde_json::json!({
        "artifact_id": artifact_id,
        "results": [{"selector": selector, "status": "ok", "text": "exact"}],
    }).to_string());
    let (receipt, _) = restored.tool_result_budget_receipt(&output, "recovery").unwrap();
    let receipt: serde_json::Value = serde_json::from_str(&receipt).unwrap();
    assert_eq!(receipt["recovery"]["artifact_id"], artifact_id);
    assert_eq!(receipt["recovery"]["selectors"], serde_json::json!([selector]));
    let pin: serde_json::Value = serde_json::from_str(&restored.artifact_pin_payload_for_items(&[
        function_call("origin"), text_output("origin", bounded_output())
    ]).unwrap()).unwrap();
    assert_eq!(pin["artifacts"][0]["recovered_selectors"], serde_json::json!([selector]));
}

#[test]
fn verified_evidence_oversized_pin_does_not_block_later_handles() {
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for id in ["small-a", "small-b", "large"] {
        let mut record = candidate(id, bounded_output());
        record.artifact_id = format!("artifact-{id}");
        let artifact = record.artifact_id.clone();
        state.register(record);
        items.push(text_output(id, bounded_output()));
        if id == "large" {
            state.recovered_ranges.insert(id.into(), (0..1000).map(|n|
                serde_json::json!({"kind":"bytes", "start":n * 2, "end":n * 2 + 1})
            ).collect());
        }
        assert!(state.artifact_call_ids.contains_key(&artifact));
    }
    let rendered = state.artifact_pin_payload_for_items(&items).unwrap();
    let payload: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(payload["artifacts"].as_array().unwrap().len(), 3);
    assert_eq!(payload["omitted_artifact_count"], 0);
    assert!(payload["artifacts"][0].get("recovered_selectors").is_none());
    assert!(approx_token_count(&rendered) <= COMPACTION_ARTIFACT_PIN_TOKEN_BUDGET);
}

#[test]
fn factored_workspace_metadata_round_trips_every_per_call_exception() {
    let base = serde_json::json!({"observed_revision":{"repository_root":"repo","worktree_identity":"same"},
        "reason":"matching identities do not verify source dependencies", "reason_code":"workspace_freshness_unverified",
        "stale_workspace_evidence":true, "valid_for_current_workspace":false});
    let notices = (0..5).map(|index| {
        let mut notice = base.clone();
        notice["call_id"] = format!("call-{index}").into();
        notice["current_nested_results"] = serde_json::json!([{"call_id":format!("nested-{index}")}]);
        if index == 4 { notice["reason_code"] = "source_dependencies_invalidated".into(); }
        notice
    }).collect::<Vec<_>>();
    let factored = factor_workspace_notices(notices.clone());
    assert_eq!(factored["observations"].as_array().unwrap().len(), 1);
    assert_eq!(factored["notices"][4]["reason_code"], "source_dependencies_invalidated");
    assert!(factored.to_string().len() < serde_json::json!({"notices":notices}).to_string().len());
    assert_eq!(expand_workspace_notices(factored), notices);
}

#[tokio::test]
async fn overflow_directory_preserves_every_artifact_after_replacement_and_resume() {
    let (session, _) = crate::session::tests::make_session_and_context().await;
    let home = session.codex_home().await;
    let thread = session.thread_id().to_string();
    let mut state = ToolHistoryState::default();
    let mut items = Vec::new();
    for index in 0..COMPACTION_ARTIFACT_PIN_MAX_ITEMS + 5 {
        let id = format!("source-{index}");
        let mut record = candidate(&id, bounded_output());
        let canonical = CanonicalToolResult::json(serde_json::json!({"required_fact": id}));
        let artifact = create_canonical_output_artifact(home.as_path(), &thread, &canonical).await;
        assert!(artifact.complete);
        record.artifact_id = artifact.artifact_id().unwrap();
        record.artifact_bytes = canonical.exact_bytes;
        record.artifact_sha256 = canonical.sha256;
        protect_active_tool_history_artifact(home.as_path(), &thread, &record.artifact_id,
            record.artifact_bytes, &record.artifact_sha256).await.unwrap();
        state.register(record);
        items.push(text_output(&id, bounded_output()));
    }
    state.recovered_ranges.insert("source-0".into(), (0..1000).map(|index|
        serde_json::json!({"kind":"bytes","start":index*2,"end":index*2+1})).collect());
    let payload = session.compaction_artifact_pins(&state, &items).await.unwrap().unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    let directory = &payload["directory"];
    let bytes = crate::tools::command_output_artifact::read_exact_tool_output_artifact(
        session.codex_home().await.as_path(), &session.thread_id().to_string(), directory["artifact_id"].as_str().unwrap()
    ).await.unwrap();
    let recovered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(recovered["items"].as_array().unwrap().len(), COMPACTION_ARTIFACT_PIN_MAX_ITEMS + 5);
    assert_eq!(recovered["items"].as_array().unwrap().iter().find(|pin| pin["call_id"] == "source-0").unwrap()["recovered_selectors"].as_array().unwrap().len(), 1000);
    ToolHistoryMutation::RegisterArtifactOrigin {
        artifact_id: directory["artifact_id"].as_str().unwrap().to_string(),
        call_id: "context:artifact_directory".into(), bytes: directory["bytes"].as_u64().unwrap(),
        sha256: directory["sha256"].as_str().unwrap().into(),
    }.apply(&mut state);
    let replacement = vec![text_output("directory", payload.to_string())];
    ToolHistoryMutation::RegisterArtifactDirectory {
        artifact_id: directory["artifact_id"].as_str().unwrap().into(),
        members: recovered["items"].as_array().unwrap().iter()
            .map(|pin| pin["artifact_id"].as_str().unwrap().to_string()).collect(),
    }.apply(&mut state);
    let mut unrelated = candidate("created-after-directory", bounded_output());
    unrelated.artifact_id = "unrelated-later-artifact".into();
    state.register(unrelated);
    state.retain_for_history(&replacement);
    assert!(!state.candidates.contains_key("created-after-directory"));
    assert!(!state.artifact_recovery_directory().as_array().unwrap().iter()
        .any(|pin| pin["call_id"] == "context:artifact_directory"));
    let mut resumed: ToolHistoryState = serde_json::from_value(serde_json::to_value(&state).unwrap()).unwrap();
    resumed.refresh_derived_and_indexes();
    resumed = reconcile_tool_history_state(home.as_path(), &thread, resumed).await;
    resumed.retain_for_history(&replacement);
    assert_eq!(resumed.candidates.len(), COMPACTION_ARTIFACT_PIN_MAX_ITEMS + 5);
    for pin in recovered["items"].as_array().unwrap() {
        let bytes = read_exact_tool_output_artifact(home.as_path(), &thread, pin["artifact_id"].as_str().unwrap()).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["required_fact"], pin["call_id"]);
    }
    resumed.retain_for_history(&[]);
    assert!(resumed.is_persisted_empty());
}
#[test]
fn verified10_compactor_receives_decisive_evidence_not_only_checkpoint_pins() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(3000);
    let fact = format!("{}\nDECISIVE_FACT: use the canonical byte offset, not the display offset\n{}",
        "source prelude\n".repeat(80), "source suffix\n".repeat(80));
    let failure = "error: the current implementation addresses normalized bytes\n".repeat(8);
    let mut state = ToolHistoryState::default();
    let mut reviewed = candidate("reviewed", fact.clone());
    reviewed.consumed_by_generation = Some(ModelGenerationId {turn_id:"review".into(), ordinal:1});
    state.register(reviewed);
    let mut failed = candidate("failure", failure.clone());
    failed.successful = false;
    state.register(failed);
    let generic = "generic completed log\n".repeat(4000);
    state.register(candidate("generic", generic.clone()));
    for call_id in ["generic", "reviewed", "failure"] {
        state.register_non_workspace_code_mode_call(call_id.to_string());
    }
    let checkpoint = ResponseItem::Message {
        id:None, role:"assistant".into(), content:vec![codex_protocol::models::ContentItem::InputText {
            text:format!("<completed_phase_checkpoint>\n{}\n</completed_phase_checkpoint>",
                state.phase_checkpoint_receipts(&["reviewed".into()]).unwrap()),
        }], phase:None, internal_chat_message_metadata_passthrough:None,
    };
    let items = vec![function_call("generic"), text_output("generic", generic),
        function_call("reviewed"), text_output("reviewed", fact.clone()), checkpoint,
        function_call("failure"), text_output("failure", failure.clone())];
    let mut history = crate::context_manager::ContextManager::new();
    history.set_tool_history_state(state);
    history.record_items(items.iter(), TruncationPolicy::Tokens(100_000));
    let prompt = history.for_compaction_prompt_with_completed_tool_projection(
        &[codex_protocol::openai_models::InputModality::Text], None);
    let outputs = prompt.iter().filter_map(canonical_textual_output_identity).collect::<BTreeMap<_, _>>();
    assert_eq!(outputs.get("reviewed").map(|text| text.as_ref()), Some(fact.as_str()));
    assert_eq!(outputs.get("failure").map(|text| text.as_ref()), Some(failure.as_str()));
    let tokens = prompt.iter().filter_map(canonical_textual_output_identity)
        .map(|(_, text)| approx_token_count(&text)).sum::<usize>();
    assert!(tokens <= 3000, "compaction reuses its existing allowance: {tokens}");
}
#[test]
fn verified10_search_receipts_are_semantically_idempotent() {
    for omitted in [Some(0), Some(7), None] {
        let mut item = tool_search_pair("idempotent", 2000)[1].clone();
        if let ResponseItem::ToolSearchOutput { tools, omitted_result_count, .. } = &mut item {
            *tools = (0..80).map(|index| serde_json::json!({"name":format!("tool-{index}"), "description":"detail".repeat(100)})).collect();
            *omitted_result_count = omitted;
        }
        let (once, _) = tool_search_receipt_item(&item, Some(&serde_json::json!({"query":"scope"}))).unwrap();
        let (twice, _) = tool_search_receipt_item(&once, None).unwrap();
        assert_eq!(once, twice);
        let receipt = tool_search_receipt(&twice).unwrap();
        assert_eq!(receipt.result_count, 80);
        assert_eq!(receipt.omitted_result_count, omitted);
        assert_eq!(receipt.ordered_tool_identities.len() + receipt.omitted_identity_count, 80);
        let mut invalid = twice;
        if let ResponseItem::ToolSearchOutput { tools, .. } = &mut invalid { tools[0]["receipt"]["result_count"] = 1.into(); }
        assert!(tool_search_receipt_item(&invalid, None).is_none());
    }
}

#[test]
fn verified10_pressure_keeps_exact_nested_continuation_contracts() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(100);
    let states = (0..4).map(|index| serde_json::json!({
        "call_id":format!("nested-{index}"), "session_id":index + 1,
        "execution_state":"running", "process_exited":false,
        "session_capabilities":{"stdin":index == 0, "interrupt":index == 1,
            "polling":true, "cancellation":index != 1, "incarnation":format!("creation-{index}")},
        "pending_deferred_completions":[format!("job-{index}")],
        "continuation":{"tool":"write_stdin", "arguments":{"session_id":index+1, "incarnation":format!("creation-{index}"), "chars":""}}
    })).collect::<Vec<_>>();
    for output in [
        serde_json::json!({"essential":{"nested_commands":states}, "output":"log ".repeat(4000)}).to_string(),
        format!("{}\nNested command states (independent of script completion):\n{}", "log ".repeat(4000), serde_json::json!(states)),
    ] {
        let state = ToolHistoryState::default();
        let mut items = ProjectedResponseItems::Owned(vec![text_output("batch", output)]);
        assert_eq!(state.enforce_tool_result_budget(&mut items).count, 0);
        let (_, output) = canonical_textual_output_identity(&items[0]).unwrap();
        let receipt: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(receipt["control"]["nested_commands"], serde_json::json!(states));
        assert!(receipt["output_omitted"].as_bool().unwrap());
    }
}

#[test]
fn verified10_typed_receipts_distinguish_scope_and_incomplete_coverage() {
    for (path, complete) in [("owner.rs", true), ("consumer.rs", false)] {
        let mut record = candidate("read", serde_json::json!({"path":path, "file_complete":complete,
            "complete":true, "results":[{"selector":{"kind":"lines", "start":5,"end":12},
                "status":"ok", "complete":true, "text":"success ".repeat(500)}]}).to_string());
        record.tool_identity = "read_file".into();
        record.refresh_derived();
        let pin = record.artifact_pin_value().unwrap();
        assert_eq!(pin["evidence"]["path"], path);
        assert_eq!(pin["evidence"]["file_complete"], complete);
        assert_eq!(pin["evidence"]["results"][0]["selector"]["start"], 5);
        assert!(pin["evidence"]["results"][0].get("text").is_none());
        if let Some((_, receipt, _)) = record.admission_receipt() {
            let receipt: serde_json::Value = serde_json::from_str(receipt).unwrap();
            assert_eq!(receipt["evidence"], pin["evidence"]);
        }
    }
}

#[test]
fn verified10_local_compaction_retains_late_counterexample_under_pressure() {
    let _budget = override_model_visible_tool_result_token_budget_for_test(200);
    let source = format!("All selected unit tests passed.\n{}\nCounterexample: integration failed; do NOT claim complete coverage.\n{}",
        "irrelevant detail ".repeat(1500), "irrelevant tail ".repeat(1500));
    let mut record = candidate("review", source.clone());
    record.consumed_by_generation = Some(ModelGenerationId {turn_id:"review".into(), ordinal:1});
    let mut state = ToolHistoryState::default();
    state.register(record);
    state.register_non_workspace_code_mode_call("review".into());
    let mut history = crate::context_manager::ContextManager::new();
    history.set_tool_history_state(state);
    history.record_items([&function_call("review"), &text_output("review", source.clone())], TruncationPolicy::Tokens(100_000));
    let cache = GitWorkspaceCache::with_noop_watcher_for_tests();
    let prompt = history.for_local_compaction_prompt(&[codex_protocol::openai_models::InputModality::Text], None, &cache);
    assert!(prompt.contains(&text_output("review", source)));
}
#[test]
fn verified10_directory_closure_is_bounded_across_compaction_and_restart() {
    let mut state = ToolHistoryState::default();
    let mut required = candidate("required", "Keep the unresolved prohibition.".into());
    required.artifact_id = "required-artifact".into();
    state.register(required);
    let mut initial_size = None;
    for cycle in 0..8 {
        let directory = format!("directory-{cycle}");
        let context = format!("context-{cycle}");
        for (artifact_id, call_id) in [(&directory, "context:artifact_directory"), (&context, "context:plan")] {
            ToolHistoryMutation::RegisterArtifactOrigin {artifact_id:artifact_id.clone(), call_id:call_id.into(),
                bytes:1, sha256:sha256(b"x")}.apply(&mut state);
        }
        ToolHistoryMutation::RegisterArtifactDirectory {artifact_id:directory.clone(),
            members:BTreeSet::from(["required-artifact".into(), context.clone()])}.apply(&mut state);
        let mut unrelated = candidate("unrelated", "unrelated".into());
        unrelated.artifact_id = "unrelated-artifact".into();
        state.register(unrelated);
        state.retain_for_history(&[text_output("checkpoint", serde_json::json!({"artifact_id":directory}).to_string())]);
        assert_eq!(state.candidates.len(), 1);
        assert_eq!(state.internal_artifact_origins.len(), 2);
        assert_eq!(state.artifact_directory_members.len(), 1);
        let flat = state.artifact_recovery_directory();
        assert_eq!(flat.as_array().unwrap().len(), 2);
        assert!(!flat.to_string().contains("context:artifact_directory"));
        let bytes = serde_json::to_vec(&state).unwrap();
        assert!(bytes.len() <= *initial_size.get_or_insert(bytes.len()) + 32);
        state = serde_json::from_slice(&bytes).unwrap();
        state.refresh_derived_and_indexes();
    }
    state.retain_for_history(&[]);
    assert!(state.is_persisted_empty());
}
#[test]
fn continuity_read_status_scopes_keep_legacy_recursive_and_precedence() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("a.rs");
    let other = root.path().join("b.rs");
    let mut state = ToolHistoryState::default();
    for (id, scope) in [("exact", Some(SourceDependencyV1::new(&path, false))),
        ("recursive", Some(SourceDependencyV1::new(root.path(), true))), ("legacy", None)] {
        let mut entry = candidate(id, serde_json::json!({
            "path":path, "source_sha256":id, "canonical_bytes":1, "environment_id":"local",
            "canonical_uri":"file:///a.rs", "delivered_ranges":[[0,1]]
        }).to_string());
        entry.tool_identity = "read_file".into();
        entry.source_dependencies = scope.into_iter().collect();
        state.register(entry);
    }
    let expected = state.read_status(&[path.clone()], Some("local"), &[]);
    assert_eq!(expected["paths"][0]["snapshots"].as_array().unwrap().len(), 3);
    let mut unrelated = candidate("elsewhere", "{invalid JSON".into());
    unrelated.tool_identity = "read_file".into();
    unrelated.source_dependencies = BTreeSet::from([SourceDependencyV1::new(&other, false)]);
    state.register(unrelated);
    assert_eq!(state.read_status(&[path.clone()], Some("local"), &[]), expected);
    let output = text_output("exact", serde_json::json!({"path":other}).to_string());
    state.workspace_evidence.insert("exact".into(), WorkspaceEvidenceObservation::from_response_item(
        None, &output, BTreeSet::from([SourceDependencyV1::new(&other, false)])).unwrap());
    let actual = state.read_status(&[path], Some("local"), &[output]);
    assert_eq!(actual["paths"][0]["snapshots"].as_array().unwrap().len(), 2);
}

#[test]
fn continuity_registration_matches_rebuild_with_aliases_and_explicit_origins() {
    let mut state = ToolHistoryState::default();
    for id in ["z", "b", "a", "c"] {
        state.register(candidate(id, bounded_output()));
        let incremental = state.artifact_call_ids.clone();
        state.rebuild_artifact_index();
        assert_eq!(incremental, state.artifact_call_ids);
    }
    state.internal_artifact_origins.insert("artifact-1".into(), ("origin".into(), 1, "hash".into()));
    state.register(candidate("0", bounded_output()));
    assert_eq!(state.artifact_call_ids["artifact-1"], "origin");
    let incremental = state.artifact_call_ids.clone();
    state.rebuild_artifact_index();
    assert_eq!(incremental, state.artifact_call_ids);
}

