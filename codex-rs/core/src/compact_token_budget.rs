use std::sync::Arc;

use crate::compact::InitialContextInjection;
use crate::compact::SUMMARY_PREFIX;
use crate::compact::build_compacted_history;
use crate::compact::collect_user_messages;
use crate::context::world_state::WorldState;
use crate::context_manager::estimate_item_token_count;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::task_evidence::CanonicalTaskCheckpointSnapshot;
use codex_analytics::CompactionTrigger;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnStartedEvent;
use codex_utils_string::approx_token_count;
use serde::Serialize;
use sha2::Digest;
use sha2::Sha256;

const ORDINARY_CHECKPOINT_TOKEN_BUDGET: usize = 8_000;
const CURRENT_RESPONSE_STATE_TOKEN_BUDGET: i64 = 8_000;
const CURRENT_RESPONSE_STATE_SERIALIZED_BYTE_BUDGET: usize = 64 * 1024;
const CHECKPOINT_HASH_DOMAIN: &[u8] = b"codex.kd4.task-checkpoint.v1";

#[derive(Debug, Clone, Serialize)]
struct TaskCheckpointEnvelope {
    schema: &'static str,
    checkpoint_generation: String,
    semantic_hash: String,
    mandatory_context_floor_exceeded: bool,
    state: CanonicalTaskCheckpointSnapshot,
}

/// Runs token-budget manual compaction as a normal compaction lifecycle.
pub(crate) async fn run_manual_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
) -> CodexResult<()> {
    let start_event = EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: turn_context.sub_id.clone(),
        trace_id: turn_context.trace_id.clone(),
        started_at: turn_context.turn_timing_state.started_at_unix_secs().await,
        model_context_window: turn_context.model_context_window(),
        collaboration_mode_kind: turn_context.collaboration_mode.mode,
    });
    sess.send_event(&turn_context, start_event).await;

    let step_context = sess.capture_step_context(Arc::clone(&turn_context)).await;
    let world_state = Arc::new(sess.build_world_state_for_step(&step_context).await);
    run_compact_task_inner(&sess, &turn_context, world_state, CompactionTrigger::Manual).await
}

/// Runs token-budget inline auto-compaction as a normal compaction lifecycle.
pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    initial_context_injection: InitialContextInjection,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let world_state = match initial_context_injection {
        InitialContextInjection::BeforeLastUserMessage(world_state) => world_state,
        InitialContextInjection::DoNotInject => {
            Arc::new(sess.build_world_state_for_step(&step_context).await)
        }
    };
    run_compact_task_inner(&sess, turn_context, world_state, CompactionTrigger::Auto).await
}

async fn run_compact_task_inner(
    sess: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
    world_state: Arc<WorldState>,
    trigger: CompactionTrigger,
) -> CodexResult<()> {
    let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger).await;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped => return Err(CodexErr::TurnAborted),
    }

    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let history = sess.clone_history().await;
    let source_window = sess.current_window_id().await;
    let snapshot = sess
        .services
        .task_evidence
        .canonical_checkpoint_snapshot()
        .await
        .unwrap_or_else(|| empty_checkpoint_snapshot(sess.thread_id.to_string()));
    let checkpoint = build_task_checkpoint(snapshot, source_window)?;
    let retained_state = build_checkpoint_replacement_history(history.raw_items(), &checkpoint);
    sess.start_new_context_window_with_retained_evidence(
        turn_context.as_ref(),
        world_state,
        retained_state,
    )
    .await;
    sess.emit_turn_item_completed(turn_context, compaction_item)
        .await;

    let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger).await;
    if let PostCompactHookOutcome::Stopped = post_compact_outcome {
        return Err(CodexErr::TurnAborted);
    }

    Ok(())
}

fn build_checkpoint_replacement_history(
    items: &[codex_protocol::models::ResponseItem],
    checkpoint: &str,
) -> Vec<codex_protocol::models::ResponseItem> {
    let user_messages = collect_user_messages(items);
    let mut retained = build_compacted_history(Vec::new(), &user_messages, checkpoint);
    let current_turn_start = items.iter().rposition(|item| {
        matches!(
            item,
            codex_protocol::models::ResponseItem::Message { role, content, .. }
                if role == "user"
                    && !content.iter().any(|part| matches!(
                        part,
                        codex_protocol::models::ContentItem::InputText { text }
                            if crate::compact::is_summary_message(text)
                    ))
        )
    });

    // The established compaction contract rebuilds canonical initial context
    // separately. Keep provider-required compaction items plus a newest-first,
    // hard-bounded slice of response state emitted after the latest real user
    // input. Raw operational calls and payloads are intentionally represented
    // only by checkpoint proof/artifact IDs rather than being rehydrated into
    // the active prompt.
    retained.extend(
        items
            .iter()
            .filter(|item| {
                use codex_protocol::models::ResponseItem;
                matches!(
                    item,
                    ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
                )
            })
            .cloned(),
    );
    retained.extend(bounded_current_response_state(items, current_turn_start));
    retained
}

fn bounded_current_response_state(
    items: &[codex_protocol::models::ResponseItem],
    current_turn_start: Option<usize>,
) -> Vec<codex_protocol::models::ResponseItem> {
    use codex_protocol::models::ResponseItem;

    let Some(current_turn_start) = current_turn_start else {
        return Vec::new();
    };
    let mut remaining = CURRENT_RESPONSE_STATE_TOKEN_BUDGET;
    // Reserve one byte beyond each item's comma allowance so the surrounding
    // JSON array brackets also stay inside the serialized-size cap.
    let mut remaining_serialized_bytes =
        CURRENT_RESPONSE_STATE_SERIALIZED_BYTE_BUDGET.saturating_sub(1);
    let mut selected_reversed = Vec::new();
    for item in items
        .iter()
        .skip(current_turn_start.saturating_add(1))
        .rev()
    {
        let is_current_response_state = matches!(
            item,
            ResponseItem::Message { role, .. } if role == "assistant"
        ) || matches!(
            item,
            ResponseItem::AgentMessage { .. }
                | ResponseItem::Reasoning { .. }
                | ResponseItem::AdditionalTools { .. }
        );
        if !is_current_response_state {
            continue;
        }
        let item_tokens = estimate_item_token_count(item).max(0);
        let item_serialized_bytes = serde_json::to_vec(item)
            .map(|serialized| serialized.len().saturating_add(1))
            .unwrap_or(usize::MAX);
        if item_tokens > remaining || item_serialized_bytes > remaining_serialized_bytes {
            continue;
        }
        remaining = remaining.saturating_sub(item_tokens);
        remaining_serialized_bytes =
            remaining_serialized_bytes.saturating_sub(item_serialized_bytes);
        selected_reversed.push(item.clone());
    }
    selected_reversed.reverse();
    selected_reversed
}

fn empty_checkpoint_snapshot(thread_id: String) -> CanonicalTaskCheckpointSnapshot {
    use crate::task_evidence::CheckpointImplementationState;
    use std::collections::BTreeMap;

    CanonicalTaskCheckpointSnapshot {
        schema_version: 1,
        root_task_id: thread_id.clone(),
        source_thread_id: thread_id,
        evidence_revision: 0,
        provenance: BTreeMap::from([
            (
                "requirements".to_string(),
                "accepted_structured_task_contract".to_string(),
            ),
            (
                "prohibitions".to_string(),
                "accepted_structured_task_contract".to_string(),
            ),
            (
                "unresolved_conflicts".to_string(),
                "accepted_structured_task_contract".to_string(),
            ),
            (
                "decisions".to_string(),
                "active_structured_plan_task_state".to_string(),
            ),
            (
                "owners".to_string(),
                "collaboration_and_task_evidence_records".to_string(),
            ),
            (
                "relevant_files".to_string(),
                "collaboration_and_task_evidence_records".to_string(),
            ),
            (
                "relevant_contracts".to_string(),
                "active_structured_plan_task_state".to_string(),
            ),
            (
                "implementation_state".to_string(),
                "current_implementation_identity_and_receipts".to_string(),
            ),
            (
                "proof_ids".to_string(),
                "current_implementation_identity_and_receipts".to_string(),
            ),
            (
                "blockers".to_string(),
                "explicit_structured_records".to_string(),
            ),
            (
                "risks".to_string(),
                "explicit_structured_records".to_string(),
            ),
            (
                "immediate_action".to_string(),
                "active_structured_plan_task_state".to_string(),
            ),
            (
                "durable_artifact_references".to_string(),
                "existing_durable_ids_only".to_string(),
            ),
        ]),
        requirements: Vec::new(),
        prohibitions: Vec::new(),
        unresolved_conflicts: Vec::new(),
        decisions: Vec::new(),
        owners: Vec::new(),
        relevant_files: Vec::new(),
        relevant_contracts: Vec::new(),
        implementation_state: CheckpointImplementationState {
            evidence_epoch: 0,
            host_mutation_revision: 0,
            active_step_id: None,
            implementation_identities: Vec::new(),
        },
        proof_ids: Vec::new(),
        blockers: Vec::new(),
        risks: Vec::new(),
        immediate_action: None,
        durable_artifact_references: Vec::new(),
    }
}

fn build_task_checkpoint(
    mut state: CanonicalTaskCheckpointSnapshot,
    checkpoint_generation: String,
) -> CodexResult<String> {
    let semantic_hash = checkpoint_semantic_hash(&state)?;
    let mut envelope = TaskCheckpointEnvelope {
        schema: "task_checkpoint_v1",
        checkpoint_generation,
        semantic_hash,
        mandatory_context_floor_exceeded: false,
        state: state.clone(),
    };
    let mut rendered = render_checkpoint(&envelope)?;

    // Optional descriptions are deterministically replaced by the stable IDs
    // already present in the same records before considering any hard failure.
    if approx_token_count(&rendered) > ORDINARY_CHECKPOINT_TOKEN_BUDGET {
        for decision in &mut state.decisions {
            collapse_string_slot(
                &format!("decision_acceptance_criteria:{}", decision.id),
                &mut decision.acceptance_criteria,
            )?;
        }

        collapse_string_slot("owners", &mut state.owners)?;
        collapse_string_slot("relevant_files", &mut state.relevant_files)?;
        collapse_string_slot("relevant_contracts", &mut state.relevant_contracts)?;
        collapse_string_slot(
            "implementation_identities",
            &mut state.implementation_state.implementation_identities,
        )?;
        collapse_string_slot("proof_ids", &mut state.proof_ids)?;

        // Artifact IDs are deliberately never collapsed: exact retrieval
        // depends on the projected durable identifier surviving compaction
        // verbatim.
        envelope.semantic_hash = checkpoint_semantic_hash(&state)?;
        envelope.state = state;
        rendered = render_checkpoint(&envelope)?;
    }

    if approx_token_count(&rendered) > ORDINARY_CHECKPOINT_TOKEN_BUDGET {
        // Exact structured contract material is never truncated. A checkpoint
        // above the ordinary ceiling remains complete and explicitly marks the
        // mandatory floor for the 80K dispatch gate to handle numerically.
        envelope.mandatory_context_floor_exceeded = true;
        rendered = render_checkpoint(&envelope)?;
    }
    Ok(format!("{SUMMARY_PREFIX}\n{rendered}"))
}

fn collapse_string_slot(label: &str, values: &mut Vec<String>) -> CodexResult<()> {
    if values.is_empty() {
        return Ok(());
    }
    let count = values.len();
    let encoded = serde_json::to_vec(values).map_err(|error| {
        CodexErr::InvalidRequest(format!("failed to encode checkpoint {label}: {error}"))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_HASH_DOMAIN);
    hasher.update([0]);
    hasher.update(label.as_bytes());
    hasher.update([0]);
    hasher.update(encoded);
    *values = vec![format!(
        "projected:{label}:count={count}:sha256={:x}",
        hasher.finalize()
    )];
    Ok(())
}

fn checkpoint_semantic_hash(state: &CanonicalTaskCheckpointSnapshot) -> CodexResult<String> {
    let bytes = serde_json::to_vec(state).map_err(|error| {
        CodexErr::InvalidRequest(format!("failed to encode task checkpoint state: {error}"))
    })?;
    let mut hasher = Sha256::new();
    hasher.update(CHECKPOINT_HASH_DOMAIN);
    hasher.update([0]);
    hasher.update(bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

fn render_checkpoint(envelope: &TaskCheckpointEnvelope) -> CodexResult<String> {
    let json = serde_json::to_string(envelope).map_err(|error| {
        CodexErr::InvalidRequest(format!("failed to encode task checkpoint: {error}"))
    })?;
    Ok(format!("<task_checkpoint_v1>{json}</task_checkpoint_v1>"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_evidence::CheckpointReference;
    use crate::task_evidence::CheckpointRequirement;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::FunctionCallOutputBody;
    use codex_protocol::models::FunctionCallOutputPayload;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use codex_protocol::models::ResponseItem;

    #[test]
    fn checkpoint_is_deterministic_and_bounded() {
        let state = empty_checkpoint_snapshot("root".to_string());
        let first = build_task_checkpoint(state.clone(), "root:1".to_string()).unwrap();
        let second = build_task_checkpoint(state, "root:1".to_string()).unwrap();
        assert_eq!(first, second);
        assert!(first.contains("<task_checkpoint_v1>"));
        assert!(approx_token_count(&first) <= ORDINARY_CHECKPOINT_TOKEN_BUDGET);
        assert!(!first.contains("compaction_evidence_checkpoint"));
    }

    #[test]
    fn checkpoint_never_interprets_uncanonicalized_history() {
        let checkpoint = build_task_checkpoint(
            empty_checkpoint_snapshot("root".to_string()),
            "root:1".to_string(),
        )
        .unwrap();
        assert!(!checkpoint.contains("raw tool output"));
        assert!(!checkpoint.contains("hidden reasoning"));
    }

    #[test]
    fn checkpoint_has_authoritative_provenance_and_unresolved_conflicts() {
        let mut state = empty_checkpoint_snapshot("root".to_string());
        state.unresolved_conflicts.push(CheckpointReference {
            id: "source-7".to_string(),
            kind: "pending_source_classification".to_string(),
        });
        let checkpoint = build_task_checkpoint(state, "root:1".to_string()).unwrap();
        assert!(checkpoint.contains("accepted_structured_task_contract"));
        assert!(checkpoint.contains("active_structured_plan_task_state"));
        assert!(checkpoint.contains("current_implementation_identity_and_receipts"));
        assert!(checkpoint.contains("explicit_structured_records"));
        assert!(checkpoint.contains("existing_durable_ids_only"));
        assert!(checkpoint.contains("source-7"));
    }

    #[test]
    fn optional_reference_fanout_is_projected_under_the_ordinary_ceiling() {
        let mut state = empty_checkpoint_snapshot("root".to_string());
        state.relevant_files = (0..400)
            .map(|index| format!("src/{index:04}/{}", "component".repeat(20)))
            .collect();
        let checkpoint = build_task_checkpoint(state, "root:1".to_string()).unwrap();
        assert!(approx_token_count(&checkpoint) <= ORDINARY_CHECKPOINT_TOKEN_BUDGET);
        assert!(checkpoint.contains("projected:relevant_files:count=400:sha256="));
        assert!(checkpoint.contains("\"mandatory_context_floor_exceeded\":false"));
    }

    #[test]
    fn mandatory_floor_overflow_is_explicit_and_exact_material_is_not_truncated() {
        let exact_material = "mandatory-contract-material-".repeat(2_000);
        let mut state = empty_checkpoint_snapshot("root".to_string());
        state.requirements.push(CheckpointRequirement {
            id: "requirement-1".to_string(),
            source_id: "source-1".to_string(),
            source_content_hash: "hash-1".to_string(),
            exact_material: exact_material.clone(),
        });
        let checkpoint = build_task_checkpoint(state, "root:1".to_string()).unwrap();
        assert!(approx_token_count(&checkpoint) > ORDINARY_CHECKPOINT_TOKEN_BUDGET);
        assert!(checkpoint.contains("\"mandatory_context_floor_exceeded\":true"));
        assert!(checkpoint.contains(&exact_material));
    }

    #[test]
    fn repeated_checkpoint_projection_keeps_one_active_checkpoint_and_artifact_id() {
        let mut first_state = empty_checkpoint_snapshot("root".to_string());
        first_state
            .durable_artifact_references
            .push("artifact-019ff8".to_string());
        let first = build_task_checkpoint(first_state, "root:1".to_string()).unwrap();
        let history = vec![
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "current uncategorized user instruction".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText { text: first }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "current response state".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::Reasoning {
                id: None,
                summary: vec![ReasoningItemReasoningSummary::SummaryText {
                    text: "current reasoning summary".to_string(),
                }],
                content: None,
                encrypted_content: Some("opaque-reasoning".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::FunctionCallOutput {
                id: None,
                call_id: "call-raw".to_string(),
                output: FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::Text("raw-operational-payload".to_string()),
                    success: Some(true),
                },
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::ContextCompaction {
                id: None,
                encrypted_content: Some("provider-required-compaction".to_string()),
                internal_chat_message_metadata_passthrough: None,
            },
        ];
        let mut second_state = empty_checkpoint_snapshot("root".to_string());
        second_state
            .durable_artifact_references
            .push("artifact-019ff8".to_string());
        let second = build_task_checkpoint(second_state, "root:2".to_string()).unwrap();
        let projected = build_checkpoint_replacement_history(&history, &second);
        let encoded = serde_json::to_string(&projected).unwrap();
        assert_eq!(encoded.matches("<task_checkpoint_v1>").count(), 1);
        assert!(encoded.contains("current uncategorized user instruction"));
        assert!(encoded.contains("artifact-019ff8"));
        assert!(encoded.contains("current response state"));
        assert!(encoded.contains("current reasoning summary"));
        assert!(encoded.contains("provider-required-compaction"));
        assert!(!encoded.contains("raw-operational-payload"));
        assert!(!encoded.contains("root:1"));
    }

    #[test]
    fn repeated_checkpoint_projection_bounds_current_response_state() {
        let mut history = vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "long-running task".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }];
        history.extend((0..64).map(|index| ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: Some(format!("reasoning-{index}-{}", "x".repeat(4_096))),
            internal_chat_message_metadata_passthrough: None,
        }));

        let first = build_task_checkpoint(
            empty_checkpoint_snapshot("root".to_string()),
            "root:1".to_string(),
        )
        .unwrap();
        let first_projection = build_checkpoint_replacement_history(&history, &first);
        let first_response_state = first_projection
            .iter()
            .filter(|item| matches!(item, ResponseItem::Reasoning { .. }))
            .collect::<Vec<_>>();
        let first_response_tokens = first_response_state
            .iter()
            .map(|item| estimate_item_token_count(item))
            .sum::<i64>();
        let first_response_serialized_bytes =
            serde_json::to_vec(&first_response_state).unwrap().len();
        assert!(first_response_tokens <= CURRENT_RESPONSE_STATE_TOKEN_BUDGET);
        assert!(first_response_serialized_bytes <= CURRENT_RESPONSE_STATE_SERIALIZED_BYTE_BUDGET);
        assert!(first_response_state.len() < 64);
        let first_encoded = serde_json::to_string(&first_projection).unwrap();
        assert!(first_encoded.contains("reasoning-63-"));
        assert!(!first_encoded.contains("reasoning-0-"));

        let mut repeated_history = first_projection;
        repeated_history.extend((64..128).map(|index| ResponseItem::Reasoning {
            id: None,
            summary: Vec::new(),
            content: None,
            encrypted_content: Some(format!("reasoning-{index}-{}", "y".repeat(4_096))),
            internal_chat_message_metadata_passthrough: None,
        }));
        let second = build_task_checkpoint(
            empty_checkpoint_snapshot("root".to_string()),
            "root:2".to_string(),
        )
        .unwrap();
        let second_projection = build_checkpoint_replacement_history(&repeated_history, &second);
        let second_response_tokens = second_projection
            .iter()
            .filter(|item| matches!(item, ResponseItem::Reasoning { .. }))
            .map(estimate_item_token_count)
            .sum::<i64>();
        let second_response_state = second_projection
            .iter()
            .filter(|item| matches!(item, ResponseItem::Reasoning { .. }))
            .collect::<Vec<_>>();
        let second_response_serialized_bytes =
            serde_json::to_vec(&second_response_state).unwrap().len();
        assert!(second_response_tokens <= CURRENT_RESPONSE_STATE_TOKEN_BUDGET);
        assert!(second_response_serialized_bytes <= CURRENT_RESPONSE_STATE_SERIALIZED_BYTE_BUDGET);
        let second_encoded = serde_json::to_string(&second_projection).unwrap();
        assert!(second_encoded.contains("reasoning-127-"));
        assert!(!second_encoded.contains("reasoning-63-"));
    }
}
