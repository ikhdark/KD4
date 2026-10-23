use super::*;

#[tokio::test]
async fn malformed_outline_warns_without_invalidating_exact_delivered_bytes() {
    for (name, source, complete) in [
        ("broken.rs", "fn good() {}\nfn hidden(\n", false),
        ("broken.py", "def good(): pass\ndef hidden(\n", false),
        ("invalid.rs", "fn hidden(\n", false),
        ("valid.rs", "fn good() {}\nfn hidden() {}\n", true),
    ] {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join(name);
        std::fs::write(&path, source).unwrap();
        let call = super::tests::invocation(&path, json!([{"kind":"section","id":"outline"}]), false).await;
        let result = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
        assert_eq!(result["results"][0]["value"]["details"]["parse_complete"], complete);
        assert_eq!(result["results"][0]["complete"], true);
        if !complete { assert!(result["results"][0]["value"]["details"]["warning"].is_string()); }
    }
}

#[tokio::test]
async fn failed_required_protection_never_publishes_a_source_handle() {
    let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("source.txt");
    let text = "protected source evidence";
    std::fs::write(&path, text).unwrap();
    let call = super::tests::invocation(&path, json!([{"kind":"bytes","start":0,"end":4}]), false).await;
    let canonical = CanonicalToolResult::text(text);
    let home = &call.step_context.turn.config.codex_home;
    let thread = call.session.thread_id.to_string();
    let artifact = crate::tools::command_output_artifact::create_canonical_output_artifact(home, &thread, &canonical).await;
    let id = artifact.artifact_id().unwrap();
    call.session.try_register_tool_artifact_origin(id.clone(), call.call_id.clone(), canonical.exact_bytes, canonical.sha256.clone()).await.unwrap();
    std::fs::write(home.join("tool-output").join(&thread).join(format!("{id}.active-tool-history")), b"invalid marker").unwrap();
    let output = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
    assert!(output["artifact_id"].is_null());
    assert_eq!(output["retained_artifact_complete"], false);
    assert!(output["snapshot_error"].is_string());
    assert_eq!(output["results"][0]["text"], "prot");
    assert_eq!(output["results"][0]["complete"], true);
    assert!(output.get("continuation").is_none());
}

#[tokio::test]
async fn duplicate_replay_ranges_deliver_the_same_source_union_once() {
    let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("source.txt");
    let text = "x".repeat(20_000); std::fs::write(&path, &text).unwrap();
    let call = super::tests::invocation(&path, json!([{"kind":"bytes","start":0,"end":20000}]), false).await;
    let previous = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
    let ToolPayload::Function { arguments } = &call.payload else { unreachable!() };
    let request = json!({"path":path,"selectors":[{"kind":"bytes","start":0,"end":18000},
        {"kind":"bytes","start":1000,"end":19000},{"kind":"bytes","start":2000,"end":20000}]}).to_string();
    let replay = reselect_read_file_output(arguments, &request, &previous).unwrap();
    assert_eq!(replay["results"].as_array().unwrap().len(), 1);
    assert_eq!(replay["results"][0]["text"], text);
    assert_eq!(replay["source_sha256"], previous["source_sha256"]);
}

#[tokio::test]
async fn replay_miss_carries_one_current_read_and_rejects_a_changed_generation() {
    for invalid_generation in [false, true] {
        let dir = tempfile::tempdir().unwrap(); let path = dir.path().join("source.txt");
        let original = "a".repeat(339_000); let old_hash = crate::tool_history::sha256(original.as_bytes());
        let mut current = original; current.replace_range(338_999.., "b"); std::fs::write(&path, &current).unwrap();
        let call = super::tests::invocation(&path, json!([{"kind":"bytes","start":0,"end":1024}]), false).await;
        let ToolPayload::Function { arguments } = &call.payload else { unreachable!() };
        let cache = call.session.services.git_workspace.as_ref();
        let cwd = call.step_context.turn.config.cwd.to_path_buf();
        let observation = cache.begin_source_path_change_observation(&cwd, &path, false).await.unwrap();
        SOURCE_FILE_READS.with(|count| count.set(0));
        let snapshot = current_file_snapshot(&call.step_context, arguments).await.unwrap();
        assert_ne!(snapshot.sha256, old_hash);
        let hash = snapshot.sha256.clone();
        let workspace = cache.workspace_evidence_for_turn(&call.step_context.turn, &cwd).await.identity;
        let guard = crate::session::turn_execution::SuccessfulReplayGuard::for_file_snapshot_test(&call, vec![observation], workspace);
        let miss = ReadFileReplayMiss { snapshot, guard, workspace_cwd:cwd,
            mutation_revision:u64::from(invalid_generation) };
        let result = scope_read_file_replay_miss(Some(miss), ReadFileHandler.handle(call.clone())).await.unwrap().code_mode_result(&call.payload);
        assert_eq!(result["source_sha256"], hash);
        assert_eq!(SOURCE_FILE_READS.with(std::cell::Cell::get), if invalid_generation { 2 } else { 1 });
        assert_eq!(result["results"][0]["text"], "a".repeat(1024));
    }
}
