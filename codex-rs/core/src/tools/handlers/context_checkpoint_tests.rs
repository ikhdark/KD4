use super::*;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tool_history::ModelGenerationId;
use crate::tool_history::ToolHistoryCandidate;
use crate::tools::command_output_artifact::create_canonical_output_artifact;
use crate::tools::context::ToolCallSource;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_tools::CanonicalToolResult;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

async fn seed(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    id: &str,
    text: &str,
    successful: bool,
    consumed: bool,
) -> String {
    let canonical = CanonicalToolResult::text(text.to_owned());
    let artifact = create_canonical_output_artifact(
        &turn.config.codex_home,
        &session.thread_id.to_string(),
        &canonical,
    )
    .await;
    assert!(artifact.complete);
    session
        .register_tool_history_candidate(ToolHistoryCandidate {
            call_id: id.into(),
            tool_identity: "functions.exec".into(),
            semantic_class: "tool_output".into(),
            successful,
            source_dependencies: Default::default(),
            source_dependencies_current: true,
            artifact_id: artifact.artifact_id().unwrap(),
            artifact_bytes: canonical.exact_bytes,
            artifact_sha256: canonical.sha256.clone(),
            original_output_sha256: canonical.sha256,
            original_tokens: canonical.approximate_tokens,
            preserved_non_text_tokens: Some(0),
            bounded_model_output: text.into(),
            complete: true,
            projection_eligible: true,
            proof_identity: None,
            supersession_identity: None,
            consumed_by_generation: consumed.then(|| ModelGenerationId {
                turn_id: "observed".into(),
                ordinal: 0,
            }),
            derived: Default::default(),
        })
        .await;
    session
        .record_conversation_items(
            turn,
            &[
                ResponseItem::FunctionCall {
                    id: None,
                    call_id: id.into(),
                    name: "functions.exec".into(),
                    namespace: None,
                    arguments: "{}".into(),
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::FunctionCallOutput {
                    id: None,
                    call_id: id.into(),
                    output: FunctionCallOutputPayload::from_text(text.into()),
                    internal_chat_message_metadata_passthrough: None,
                },
            ],
        )
        .await
        .unwrap();
    artifact.artifact_id().unwrap()
}

async fn invoke(
    session: &Arc<Session>,
    turn: &Arc<TurnContext>,
    arguments: Value,
    cancellation: CancellationToken,
) -> Result<Value, FunctionCallError> {
    let payload = ToolPayload::Function {
        arguments: arguments.to_string(),
    };
    let result = ContextCheckpointHandler
        .handle(ToolInvocation {
            session: Arc::clone(session),
            step_context: StepContext::for_test(Arc::clone(turn)),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            cancellation_token: cancellation,
            call_id: "checkpoint-test".into(),
            tool_name: ToolName::plain("context_checkpoint"),
            source: ToolCallSource::Direct,
            payload: payload.clone(),
        })
        .await?;
    Ok(result.code_mode_result(&payload))
}

#[tokio::test]
async fn checkpoint_rejects_retained_artifact_corruption_without_persisting_notes() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.model_info.truncation_policy =
        codex_protocol::openai_models::TruncationPolicyConfig::bytes(100_000);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    seed(
        &session,
        &turn,
        "completed",
        &"completed evidence ".repeat(3000),
        true,
        true,
    )
    .await;
    let retained = seed(
        &session,
        &turn,
        "retained",
        "essential evidence",
        false,
        false,
    )
    .await;
    let path = turn
        .config
        .codex_home
        .join("tool-output")
        .join(session.thread_id.to_string())
        .join(format!("{retained}.log"));
    std::fs::write(path, "corrupted evidence").unwrap();
    let before = session.clone_history().await.raw_items().to_vec();
    let error = invoke(&session, &turn, json!({"summary":"done", "active_work":"repair failure", "completed_call_ids":["completed"], "retained_evidence":["retained"]}), CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("not recoverable"), "{error}");
    assert_eq!(session.clone_history().await.raw_items(), before);
}

#[tokio::test]
async fn checkpoint_admission_is_profitable_idempotent_and_preserves_failures() {
    let (session, mut turn) = crate::session::tests::make_session_and_context().await;
    turn.model_info.truncation_policy =
        codex_protocol::openai_models::TruncationPolicyConfig::bytes(100_000);
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    for (id, repeats, successful, consumed) in [
        ("tiny", 1, true, true),
        ("small", 600, true, true),
        ("large", 6000, true, true),
        ("active", 600, true, true),
        ("failed", 6000, false, true),
        ("unread", 6000, true, false),
        ("persist", 6000, true, true),
    ] {
        seed(
            &session,
            &turn,
            id,
            &"evidence ".repeat(repeats),
            successful,
            consumed,
        )
        .await;
    }
    let args = |ids: Value, retained: Value, notes: &str| {
        json!({
            "summary":notes,"active_work":notes,"completed_call_ids":ids,"retained_evidence":retained,
        })
    };
    let before = session.clone_history().await.raw_items().to_vec();
    for notes in ["", " \t\r\n"] {
        let result = invoke(
            &session,
            &turn,
            args(json!([]), json!([]), notes),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result["reason"], "empty_checkpoint");
        assert_eq!(result["checkpoint_item_persisted"], false);
        assert_eq!(session.clone_history().await.raw_items(), before);
    }
    for invalid in [
        args(json!(["large"]), json!([]), &"x".repeat(8193)),
        args(json!(["large"]), json!(["x".repeat(257)]), "notes"),
        args(
            json!(["large"]),
            json!([]),
            &"x".repeat(MAX_CHECKPOINT_BYTES),
        ),
    ] {
        assert!(
            invoke(&session, &turn, invalid, CancellationToken::new())
                .await
                .is_err()
        );
        assert_eq!(session.clone_history().await.raw_items(), before);
    }
    for retained in [vec!["id".to_string(); 129], vec!["é".repeat(128); 33]] {
        let error = invoke(
            &session,
            &turn,
            args(json!(["large"]), json!(retained), "notes"),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("8192 bytes"));
        assert_eq!(session.clone_history().await.raw_items(), before);
    }
    let response = invoke(
        &session,
        &turn,
        args(json!(["small"]), json!([]), &"remaining work ".repeat(400)),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(response["reason"], "insufficient_net_savings");
    assert_eq!(response["changed"], false);
    assert_eq!(session.clone_history().await.raw_items(), before);

    for (ids, retained) in [
        (json!(["missing"]), json!([])),
        (json!(["failed"]), json!([])),
        (json!(["unread"]), json!([])),
        (json!(["large"]), json!(["missing"])),
        (json!(["large"]), json!(["large"])),
    ] {
        assert!(
            invoke(
                &session,
                &turn,
                args(ids, retained, "notes"),
                CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert_eq!(session.clone_history().await.raw_items(), before);
    }
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(
        invoke(
            &session,
            &turn,
            args(json!(["large"]), json!([]), "notes"),
            cancelled
        )
        .await
        .is_err()
    );
    assert_eq!(session.clone_history().await.raw_items(), before);

    let accepted = invoke(
        &session,
        &turn,
        args(json!(["large", "large", "tiny"]), json!(["active"]), "done"),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(accepted["checkpointed_call_count"], 1);
    assert!(accepted.get("checkpointed_call_ids").is_none());
    assert_eq!(accepted["changed"], true);
    assert_eq!(accepted["checkpoint_item_persisted"], true);
    let after = session.clone_history().await.raw_items().to_vec();
    assert_eq!(&after[..before.len()], before);
    assert_eq!(after.len(), before.len() + 1);
    let repeated = invoke(
        &session,
        &turn,
        args(json!(["large"]), json!(["active"]), "different notes"),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(repeated["reason"], "already_checkpointed");
    assert_eq!(repeated["changed"], false);
    assert_eq!(session.clone_history().await.raw_items(), after);

    session.close_durable_history_commit_gate_for_test();
    let failed = invoke(
        &session,
        &turn,
        args(json!(["persist"]), json!([]), "done"),
        CancellationToken::new(),
    )
    .await;
    assert!(failed.is_err());
    assert_eq!(session.clone_history().await.raw_items(), after);
}

#[tokio::test]
async fn checkpoint_rejects_retention_that_cannot_survive_compaction() {
    let (session, turn) = crate::session::tests::make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    seed(
        &session,
        &turn,
        "completed",
        &"evidence ".repeat(2000),
        true,
        true,
    )
    .await;
    let mut retained = Vec::new();
    for index in 0..40 {
        let id = format!("retained-{index}");
        seed(
            &session,
            &turn,
            &id,
            &format!("essential evidence {index}"),
            false,
            false,
        )
        .await;
        retained.push(id);
    }
    let before = session.clone_history().await.raw_items().to_vec();
    let error = invoke(&session, &turn, json!({"summary":"done", "active_work":"repair", "completed_call_ids":["completed"], "retained_evidence":retained}), CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("recovery-pin budget"), "{error}");
    assert_eq!(session.clone_history().await.raw_items(), before);
}
