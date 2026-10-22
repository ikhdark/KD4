use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;

use crate::model::ConversationItemKind;
use crate::model::ExecutionStatus;
use crate::payload::RawPayloadKind;
use crate::raw_event::RawTraceEventPayload;
use crate::reducer::test_support::append_inference_start;
use crate::reducer::test_support::create_started_writer;
use crate::reducer::test_support::message;
use crate::reducer::test_support::start_turn;
use crate::replay_bundle;

#[test]
fn delayed_success_refines_owner_cleanup_without_reopening_the_turn() -> anyhow::Result<()> {
    for delayed_start in [false, true] {
        let temp = TempDir::new()?;
        let writer = create_started_writer(&temp)?;
        start_turn(&writer, "turn-1")?;
        let request = writer.write_json_payload(
            RawPayloadKind::InferenceRequest,
            &json!({"input": [message("user", "request")]}),
        )?;
        if !delayed_start {
            append_inference_start(&writer, "attempt", "turn-1", request.clone())?;
        }
        let owner_end = writer.append(RawTraceEventPayload::CodexTurnEnded {
            codex_turn_id: "turn-1".into(),
            status: ExecutionStatus::Completed,
        })?;
        if delayed_start {
            append_inference_start(&writer, "attempt", "turn-1", request)?;
        }
        let partial = replay_bundle(temp.path())?;
        assert_eq!(
            partial.inference_calls["attempt"].execution.status,
            ExecutionStatus::Cancelled
        );
        assert_eq!(
            partial.inference_calls["attempt"].observed_terminal_status,
            None
        );
        let completed = writer.append(RawTraceEventPayload::InferenceCompleted {
            inference_call_id: "attempt".into(),
            response_id: Some("response".into()),
            upstream_request_id: None,
            response_payload: None,
        })?;
        writer.append(RawTraceEventPayload::InferenceFailed {
            inference_call_id: "attempt".into(),
            upstream_request_id: None,
            response_id: None,
            error: "conflicting duplicate".into(),
            partial_response_payload: None,
        })?;
        let rollout = replay_bundle(temp.path())?;
        let inference = &rollout.inference_calls["attempt"];
        assert_eq!(inference.execution.status, ExecutionStatus::Completed);
        assert_eq!(
            inference.observed_terminal_status,
            Some(ExecutionStatus::Completed)
        );
        assert_eq!(inference.execution.ended_seq, Some(completed.seq));
        assert_eq!(
            inference.owner_end.as_ref().unwrap().ended_seq,
            Some(owner_end.seq)
        );
        assert_eq!(
            rollout.codex_turns["turn-1"].execution.ended_seq,
            Some(owner_end.seq)
        );
    }
    Ok(())
}

#[test]
fn inference_occurrence_survives_deferred_persistence() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let writer = std::sync::Arc::new(create_started_writer(&temp)?);
    start_turn(&writer, "turn-1")?;
    let attempt = crate::InferenceTraceContext::enabled(
        writer,
        crate::reducer::test_support::ROOT_THREAD_ID.into(),
        "turn-1".into(),
        "model".into(),
        "provider".into(),
    )
    .start_attempt();
    let started = crate::TraceOccurrence {
        clock_id: "independent-clock".into(),
        offset_us: 100,
    };
    let ended = crate::TraceOccurrence {
        clock_id: "independent-clock".into(),
        offset_us: 900,
    };
    attempt
        .at_occurrence(started.clone())
        .record_started(&json!({"input": []}));
    attempt
        .at_occurrence(ended.clone())
        .record_completed("response", None, &None, &[]);
    let rollout = replay_bundle(temp.path())?;
    let inference = rollout.inference_calls.values().next().unwrap();
    assert_eq!(
        rollout.event_occurrences[&inference.execution.started_seq],
        started
    );
    assert_eq!(
        rollout.event_occurrences[&inference.execution.ended_seq.unwrap()],
        ended
    );
    Ok(())
}

#[test]
fn cancelled_inference_reduces_partial_response_items() -> anyhow::Result<()> {
    let temp = TempDir::new()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "turn-1")?;

    let request = writer.write_json_payload(
        RawPayloadKind::InferenceRequest,
        &json!({
            "input": [message("user", "draft")]
        }),
    )?;
    append_inference_start(&writer, "inference-1", "turn-1", request)?;

    let partial_response = writer.write_json_payload(
        RawPayloadKind::InferenceResponse,
        &json!({
            "response_id": null,
            "token_usage": null,
            "output_items": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "partial"}]
            }]
        }),
    )?;
    writer.append(RawTraceEventPayload::InferenceCancelled {
        inference_call_id: "inference-1".to_string(),
        upstream_request_id: Some("req-cancelled".to_string()),
        reason: "test interruption".to_string(),
        partial_response_payload: Some(partial_response),
    })?;

    let rollout = replay_bundle(temp.path())?;
    let inference = &rollout.inference_calls["inference-1"];
    let response_item_id = &inference.response_item_ids[0];

    assert_eq!(inference.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(
        inference.upstream_request_id,
        Some("req-cancelled".to_string()),
    );
    assert_eq!(inference.response_item_ids.len(), 1);
    assert_eq!(
        rollout.conversation_items[response_item_id].kind,
        ConversationItemKind::Message,
    );
    assert_eq!(
        rollout.conversation_items[response_item_id].produced_by,
        vec![crate::model::ProducerRef::Inference {
            inference_call_id: "inference-1".to_string(),
        }],
    );

    Ok(())
}

#[test]
fn late_cancelled_inference_preserves_turn_end_status() -> anyhow::Result<()> {
    for (turn_status, expected_status) in [
        (ExecutionStatus::Completed, ExecutionStatus::Cancelled),
        (ExecutionStatus::Cancelled, ExecutionStatus::Cancelled),
        (ExecutionStatus::Failed, ExecutionStatus::Failed),
        (ExecutionStatus::Aborted, ExecutionStatus::Aborted),
    ] {
        let temp = TempDir::new()?;
        let writer = create_started_writer(&temp)?;
        start_turn(&writer, "turn-1")?;

        let request = writer.write_json_payload(
            RawPayloadKind::InferenceRequest,
            &json!({
                "input": [message("user", "interrupt")]
            }),
        )?;
        append_inference_start(&writer, "inference-1", "turn-1", request)?;
        let turn_end = writer.append(RawTraceEventPayload::CodexTurnEnded {
            codex_turn_id: "turn-1".to_string(),
            status: turn_status,
        })?;

        let before_late_response = replay_bundle(temp.path())?;
        let inference = &before_late_response.inference_calls["inference-1"];
        assert_eq!(inference.execution.status, expected_status);
        assert_eq!(inference.execution.ended_seq, Some(turn_end.seq));
        assert_eq!(
            inference.execution.ended_at_unix_ms,
            Some(turn_end.wall_time_unix_ms)
        );
        assert!(inference.response_item_ids.is_empty());
        assert!(inference.observed_terminal.is_none());
        assert_eq!(
            inference.owner_end.as_ref().unwrap().ended_seq,
            Some(turn_end.seq)
        );

        let partial_response = writer.write_json_payload(
            RawPayloadKind::InferenceResponse,
            &json!({
                "response_id": null,
                "token_usage": null,
                "output_items": [{
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "late partial"}]
                }]
            }),
        )?;
        let observed_terminal = writer.append(RawTraceEventPayload::InferenceCancelled {
            inference_call_id: "inference-1".to_string(),
            upstream_request_id: Some("req-late-cancelled".to_string()),
            reason: "stream mapper noticed cancellation after turn end".to_string(),
            partial_response_payload: Some(partial_response.clone()),
        })?;

        let rollout = replay_bundle(temp.path())?;
        let inference = &rollout.inference_calls["inference-1"];
        assert_eq!(inference.execution.status, expected_status);
        assert_eq!(inference.execution.ended_seq, Some(turn_end.seq));
        assert_eq!(
            inference.observed_terminal.as_ref().unwrap().ended_seq,
            Some(observed_terminal.seq)
        );
        assert_eq!(
            inference.raw_response_payload_id,
            Some(partial_response.raw_payload_id),
        );
        assert_eq!(
            inference.upstream_request_id,
            Some("req-late-cancelled".to_string()),
        );
        assert_eq!(inference.response_item_ids.len(), 1);
        let response_item_id = &inference.response_item_ids[0];
        assert_eq!(
            rollout.conversation_items[response_item_id].body.parts,
            vec![crate::model::ConversationPart::Text {
                text: "late partial".to_string(),
            }],
        );
    }
    Ok(())
}

#[test]
fn late_cancelled_response_does_not_contaminate_new_incremental_context() -> anyhow::Result<()> {
    use crate::reducer::test_support::append_inference_completion;
    let temp = TempDir::new()?;
    let writer = create_started_writer(&temp)?;
    start_turn(&writer, "old-turn")?;
    let old_request = writer.write_json_payload(
        RawPayloadKind::InferenceRequest,
        &json!({"input": [message("user", "old")]}),
    )?;
    append_inference_start(&writer, "old", "old-turn", old_request)?;
    writer.append(RawTraceEventPayload::CodexTurnEnded {
        codex_turn_id: "old-turn".to_string(),
        status: ExecutionStatus::Cancelled,
    })?;
    start_turn(&writer, "new-turn")?;
    let request = writer.write_json_payload(
        RawPayloadKind::InferenceRequest,
        &json!({"input": [message("user", "new")]}),
    )?;
    append_inference_start(&writer, "new", "new-turn", request)?;
    let response = writer.write_json_payload(
        RawPayloadKind::InferenceResponse,
        &json!({"output_items": [message("assistant", "new answer")]}),
    )?;
    append_inference_completion(&writer, "new", "new-response", response)?;
    let partial = writer.write_json_payload(
        RawPayloadKind::InferenceResponse,
        &json!({"output_items": [message("assistant", "old partial")]}),
    )?;
    writer.append(RawTraceEventPayload::InferenceCancelled {
        inference_call_id: "old".to_string(),
        upstream_request_id: None,
        reason: "cancelled".to_string(),
        partial_response_payload: Some(partial),
    })?;
    let followup = writer.write_json_payload(
        RawPayloadKind::InferenceRequest,
        &json!({"previous_response_id": "new-response", "input": [message("user", "continue")]}),
    )?;
    append_inference_start(&writer, "followup", "new-turn", followup)?;
    let rollout = replay_bundle(temp.path())?;
    let old = &rollout.inference_calls["old"];
    let new = &rollout.inference_calls["new"];
    let followup = &rollout.inference_calls["followup"].request_item_ids;
    assert_eq!(old.execution.status, ExecutionStatus::Cancelled);
    assert_eq!(old.response_item_ids.len(), 1);
    assert_eq!(followup.len(), 3);
    assert_eq!(
        followup[..2],
        [
            new.request_item_ids[0].clone(),
            new.response_item_ids[0].clone()
        ]
    );
    assert!(!followup.contains(&old.response_item_ids[0]));
    Ok(())
}
