use super::*;

#[tokio::test]
async fn new_source_snapshot_registers_without_a_second_digest_verification() {
    use crate::tools::command_output_artifact::PROTECTION_VERIFICATIONS;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    std::fs::write(&path, "complete source snapshot\r\n").unwrap();
    let call = super::tests::invocation(
        &path, json!([{"kind":"bytes","start":0,"end":8}]), false,
    ).await;
    PROTECTION_VERIFICATIONS.with(|count| count.set(0));
    let first = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
    assert_eq!(first["retained_artifact_complete"], true);
    assert_eq!(first["results"][0]["text"], "complete");
    assert_eq!(PROTECTION_VERIFICATIONS.with(std::cell::Cell::get), 0);
    let second = ReadFileHandler.handle(call.clone()).await.unwrap().code_mode_result(&call.payload);
    assert_eq!(second["artifact_id"], first["artifact_id"]);
    assert_eq!(PROTECTION_VERIFICATIONS.with(std::cell::Cell::get), 1,
        "attached snapshots must still authenticate inherited bytes");
}

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




