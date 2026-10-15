use super::*;
use codex_protocol::protocol::ThreadHistoryMode;
use pretty_assertions::assert_eq;
use serde_json::json;

// Expected payloads come from the protocol contracts, not the persistence filter.
fn activity_events() -> Vec<EventMsg> {
    vec![
        json!({"type":"turn_phase_changed","turn_id":"turn","phases":["workspaceGate","delivery"]}),
        json!({"type":"safety_buffering","model":"test","use_cases":["test"],"reasons":["waiting"],"show_buffering_ui":true,"faster_model":null}),
        json!({"type":"agent_reasoning_section_break","item_id":"reasoning","summary_index":1}),
        json!({"type":"reasoning_content_delta","thread_id":"thread","turn_id":"turn","item_id":"reasoning","delta":"Compare alternatives.","summary_index":1}),
        json!({"type":"reasoning_raw_content_delta","thread_id":"thread","turn_id":"turn","item_id":"reasoning","delta":"Provider-supplied content.","content_index":2}),
        json!({"type":"plan_delta","thread_id":"thread","turn_id":"turn","item_id":"plan","delta":"1. Inspect\n2. Implement"}),
        json!({"type":"agent_message_content_delta","thread_id":"thread","turn_id":"turn","item_id":"message","delta":"Implementing the change."}),
        json!({"type":"patch_apply_updated","call_id":"patch","changes":{}}),
        json!({"type":"turn_diff","unified_diff":"--- a/file\n+++ b/file\n@@ -1 +1 @@\n-old\n+new\n"}),
        json!({"type":"mcp_tool_call_progress","call_id":"mcp","message":"halfway","progress":1.0,"total":2.0}),
        // Preserve arbitrary output bytes, including invalid UTF-8, through JSONL.
        json!({"type":"exec_command_output_delta","call_id":"exec","stream":"stdout","chunk":"/wAB"}),
        json!({"type":"terminal_interaction","call_id":"exec","process_id":"123","stdin":"continue\n"}),
    ]
    .into_iter()
    .map(|value| serde_json::from_value(value).expect("valid protocol fixture"))
    .collect()
}

#[tokio::test]
async fn rollout_activity_is_opt_in_and_preserves_every_payload_without_changing_delivery() {
    let (mut session, _turn, rx) = make_session_and_context_with_rx().await;
    let path =
        attach_thread_persistence(Arc::get_mut(&mut session).expect("unique test session")).await;
    assert!(!session.features.enabled(Feature::RolloutActivity));
    let activity = activity_events();
    for msg in &activity {
        session
            .send_event_raw(Event {
                id: "turn".into(),
                msg: msg.clone(),
            })
            .await;
        assert_eq!(
            serde_json::to_value(rx.recv().await.unwrap().msg).unwrap(),
            serde_json::to_value(msg).unwrap(),
            "capture must not affect live delivery"
        );
    }
    session.flush_rollout().await.unwrap();
    let (disabled, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    assert!(!disabled.iter().any(|item| matches!(item,
        RolloutItem::EventMsg(event) if codex_rollout::is_rollout_activity_event(event))));

    Arc::get_mut(&mut session)
        .expect("unique test session")
        .features
        .enable(Feature::RolloutActivity)
        .unwrap();
    for msg in &activity {
        session
            .send_event_raw(Event {
                id: "turn".into(),
                msg: msg.clone(),
            })
            .await;
        assert_eq!(
            serde_json::to_value(rx.recv().await.unwrap().msg).unwrap(),
            serde_json::to_value(msg).unwrap()
        );
    }
    // Replay-only/non-persisting delivery must remain non-persisting even when enabled.
    session
        .send_event_raw_with_persistence(
            Event {
                id: "turn".into(),
                msg: activity[0].clone(),
            },
            false,
        )
        .await;
    rx.recv().await.unwrap();
    let plan = EventMsg::PlanUpdate(codex_protocol::plan_tool::UpdatePlanArgs {
        explanation: Some("Existing canonical plan is retained exactly once.".into()),
        plan: Vec::new(),
    });
    session
        .send_event_raw(Event {
            id: "turn".into(),
            msg: plan.clone(),
        })
        .await;
    rx.recv().await.unwrap();
    session.flush_rollout().await.unwrap();
    let (enabled, _, errors) = RolloutRecorder::load_rollout_items(&path).await.unwrap();
    assert_eq!(errors, 0);
    let actual: Vec<_> = enabled
        .into_iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(event) => Some(event),
            _ => None,
        })
        .collect();
    let mut expected = activity;
    expected.push(plan);
    assert_eq!(
        serde_json::to_value(actual).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    session.live_thread().unwrap().shutdown().await.unwrap();
}

#[test]
fn rollout_activity_policy_and_telemetry_agree_in_both_history_modes() {
    let events = activity_events();
    let items: Vec<_> = events.into_iter().map(RolloutItem::EventMsg).collect();
    for mode in [ThreadHistoryMode::Legacy, ThreadHistoryMode::Paginated] {
        for enabled in [false, true] {
            let expected = if enabled { items.clone() } else { Vec::new() };
            let filtered =
                codex_rollout::persisted_rollout_items_with_activity(&items, mode, enabled);
            let (measured, measurement) =
                codex_rollout::measure_and_filter_rollout_items_with_activity(
                    &items, mode, enabled,
                );
            assert_eq!(
                serde_json::to_value(filtered).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
            assert_eq!(
                serde_json::to_value(measured).unwrap(),
                serde_json::to_value(&expected).unwrap()
            );
            assert_eq!(measurement.post_filter.items as usize, expected.len());
            let expected_bytes: usize = expected
                .iter()
                .map(|item| serde_json::to_vec(item).unwrap().len())
                .sum();
            assert_eq!(measurement.post_filter.payload_bytes, expected_bytes as u64);
        }
    }
}

#[test]
fn rollout_activity_does_not_change_model_or_completed_thread_history() {
    let canonical =
        vec![
        RolloutItem::ResponseItem(user_message("Implement the requested change.")),
        RolloutItem::EventMsg(serde_json::from_value(json!({
            "type":"agent_message","message":"Implemented and checked.","phase":"final_answer"
        })).unwrap()),
    ];
    let mut with_activity = vec![canonical[0].clone()];
    with_activity.extend(activity_events().into_iter().map(RolloutItem::EventMsg));
    with_activity.push(canonical[1].clone());
    assert_eq!(
        serde_json::to_value(Session::reconstruct_model_history_from_rollout(
            &with_activity
        ))
        .unwrap(),
        serde_json::to_value(Session::reconstruct_model_history_from_rollout(&canonical)).unwrap(),
    );
    // Implicit turns are addressed by their rollout position, which persisted
    // activity legitimately shifts; everything else must be unchanged.
    let turns_without_ids = |items: &[RolloutItem]| {
        let mut turns = codex_app_server_protocol::build_turns_from_rollout_items(items);
        for turn in &mut turns {
            turn.id.clear();
        }
        turns
    };
    assert_eq!(
        turns_without_ids(&with_activity),
        turns_without_ids(&canonical),
    );
}
