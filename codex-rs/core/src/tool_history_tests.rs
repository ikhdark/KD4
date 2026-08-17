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
        artifact_id: "artifact-1".to_string(),
        artifact_bytes: 96_000,
        artifact_sha256: sha256(b"canonical artifact"),
        original_output_sha256: sha256(b"raw output before bounding"),
        original_tokens: 24_000,
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

    assert_eq!(projection.unreplaced_items, canonical);
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
fn tool_history_receipt_requires_consumed_complete_matching_bounded_output() {
    let call_id = "call-1";
    let bounded = bounded_output();
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
