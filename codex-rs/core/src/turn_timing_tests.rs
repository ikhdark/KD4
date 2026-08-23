use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use codex_analytics::TurnProfile;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::DeterministicContinuationClass;
use codex_protocol::protocol::DeterministicContinuationHostAction;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::ToolLifecycleBoundary;
use codex_protocol::protocol::ToolLifecycleContext;
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingAttemptKind;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_protocol::protocol::TurnTimingGenerationDisposition;
use codex_protocol::protocol::TurnTimingGenerationPurpose;
use codex_protocol::protocol::TurnTimingGenerationReason;
use codex_protocol::protocol::TurnTimingProgressKind;
use codex_protocol::protocol::TurnTimingToolCallSource;
use codex_protocol::protocol::TurnTimingToolLifecycleEvent;
use pretty_assertions::assert_eq;

use super::ClockSample;
use super::ContinuationCause;
use super::InteractiveWaitKind;
use super::MAX_MODEL_REQUEST_PHYSICAL_ATTEMPT_IDS;
use super::MAX_MODEL_REQUEST_PROGRESS_KINDS;
use super::MAX_MODEL_REQUEST_TIMINGS;
use super::RESERVED_TOOL_OUTPUT_RECURSIVE_SPILL_COUNT;
use super::TimeSample;
use super::TurnClock;
use super::TurnLocalPhase;
use super::TurnTimingState;
use super::response_event_records_actionable_output;
use super::response_item_records_model_output;
use super::response_item_records_visible_output;
use crate::ResponseEvent;
use crate::tools::tool_dispatch_trace::ToolDispatchTimingSnapshot;

const NS_PER_MS: u128 = 1_000_000;

#[derive(Debug)]
struct FakeClock {
    sample: Mutex<TimeSample>,
}

impl FakeClock {
    fn new(monotonic_ns: u128, wall_unix_ms: i64) -> Self {
        Self {
            sample: Mutex::new(TimeSample {
                monotonic_ns,
                wall_unix_ms,
            }),
        }
    }

    fn set(&self, monotonic_ns: u128, wall_unix_ms: i64) {
        *self.sample.lock().expect("fake clock lock") = TimeSample {
            monotonic_ns,
            wall_unix_ms,
        };
    }

    fn set_ms(&self, monotonic_ms: u128) {
        let mut sample = self.sample.lock().expect("fake clock lock");
        sample.monotonic_ns = monotonic_ms.saturating_mul(NS_PER_MS);
        sample.wall_unix_ms = i64::try_from(monotonic_ms).unwrap_or(i64::MAX);
    }
}

impl TurnClock for FakeClock {
    fn sample(&self) -> ClockSample {
        ClockSample {
            time: *self.sample.lock().expect("fake clock lock"),
        }
    }
}

fn timing() -> (Arc<FakeClock>, Arc<TurnTimingState>) {
    let clock = Arc::new(FakeClock::new(0, 0));
    let state = Arc::new(TurnTimingState::with_clock(clock.clone()));
    (clock, state)
}

#[test]
fn legacy_turn_timing_deserializes_with_empty_terminalization_phases() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    let timing = state.complete_snapshot().protocol_timing();
    let mut legacy = serde_json::to_value(timing).expect("serialize timing");
    legacy
        .as_object_mut()
        .expect("timing object")
        .remove("terminalization");
    legacy
        .as_object_mut()
        .expect("timing object")
        .remove("toolCalls");
    legacy
        .as_object_mut()
        .expect("timing object")
        .remove("toolCallTimingOverflow");

    let restored: TurnTiming = serde_json::from_value(legacy).expect("legacy timing");
    assert_eq!(restored.terminalization, Default::default());
    assert!(restored.tool_calls.is_empty());
    assert_eq!(restored.tool_call_timing_overflow, 0);
}

#[test]
fn schema_ten_planning_digest_fields_are_ignored_on_deserialization() {
    let (clock, state) = timing();
    state.mark_turn_started();
    clock.set_ms(1);
    state.mark_model_request_dispatched();
    clock.set_ms(2);
    let _ =
        state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("x".to_string()));

    let mut legacy = serde_json::to_value(state.complete_snapshot().protocol_timing())
        .expect("serialize timing");
    legacy["schemaVersion"] = serde_json::json!(10);
    legacy["counters"]["planningRepeatedDigestCount"] = serde_json::json!(2);
    legacy["preFirstModelOutput"]["planningIdentityNs"] = serde_json::json!(17);

    let restored: TurnTiming = serde_json::from_value(legacy).expect("schema ten timing");
    assert_eq!(restored.schema_version, 10);
    let reserialized = serde_json::to_value(restored).expect("reserialize timing");
    assert!(
        reserialized["counters"]
            .get("planningRepeatedDigestCount")
            .is_none()
    );
    assert!(
        reserialized["preFirstModelOutput"]
            .get("planningIdentityNs")
            .is_none()
    );
}

#[test]
fn turn_timing_state_records_visible_output_only_once() {
    let (clock, state) = timing();
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("hi".to_string())),
        None
    );

    state.mark_turn_started();
    clock.set_ms(10);
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::Created),
        None
    );
    clock.set_ms(20);
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("hi".to_string())),
        Some(Duration::from_millis(20))
    );
    clock.set_ms(30);
    assert_eq!(
        state
            .record_response_event_milestones(&ResponseEvent::OutputTextDelta("again".to_string())),
        None
    );
}

#[test]
fn pre_first_model_output_snapshot_is_atomic_and_immutable() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(1);
    let history = state.begin_local_phase(TurnLocalPhase::HistorySnapshot);
    clock.set_ms(4);
    drop(history);
    let normalization = state.begin_local_phase(TurnLocalPhase::Normalization);
    clock.set_ms(6);
    drop(normalization);
    clock.set_ms(10);
    state.mark_model_request_dispatched();

    clock.set_ms(15);
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("hi".to_string())),
        Some(Duration::from_millis(15))
    );

    clock.set_ms(20);
    let later_work = state.begin_local_phase(TurnLocalPhase::PromptConstruction);
    clock.set_ms(25);
    drop(later_work);
    clock.set_ms(30);
    assert_eq!(
        state
            .record_response_event_milestones(&ResponseEvent::OutputTextDelta("again".to_string())),
        None
    );

    let snapshot = state.complete_snapshot();
    let pre_output = snapshot
        .profile
        .pre_first_model_output
        .expect("substantive model output should freeze the snapshot");
    assert_eq!(pre_output.captured_at_ns, 15 * NS_PER_MS);
    assert_eq!(pre_output.first_request_dispatch_ready_ns, 10 * NS_PER_MS);
    assert_eq!(pre_output.client_critical_path_ns, 10 * NS_PER_MS);
    assert_eq!(pre_output.attributed_client_union_ns, 5 * NS_PER_MS);
    assert_eq!(pre_output.unattributed_pre_output_ns, 5 * NS_PER_MS);
    assert_eq!(pre_output.history_snapshot_ns, 3 * NS_PER_MS);
    assert_eq!(pre_output.normalization_ns, 2 * NS_PER_MS);
    assert_eq!(pre_output.prompt_construction_ns, 0);
}

#[test]
fn pending_turn_preparation_is_attributed_before_first_model_output() {
    let (clock, state) = timing();
    state.mark_turn_started();
    let preparation = state.begin_local_phase(TurnLocalPhase::Preparation);

    clock.set_ms(2);
    let planning = state.begin_local_phase(TurnLocalPhase::Planning);
    clock.set_ms(6);
    let router = state.begin_local_phase(TurnLocalPhase::RouterBuild);
    clock.set_ms(8);
    drop(router);
    clock.set_ms(9);
    drop(planning);
    clock.set_ms(10);
    drop(preparation);
    state.mark_model_request_dispatched();

    clock.set_ms(15);
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("hi".to_string())),
        Some(Duration::from_millis(15))
    );

    let pre_output = state
        .complete_snapshot()
        .profile
        .pre_first_model_output
        .expect("substantive model output should freeze the snapshot");
    assert_eq!(pre_output.client_critical_path_ns, 10 * NS_PER_MS);
    assert_eq!(pre_output.attributed_client_union_ns, 10 * NS_PER_MS);
    assert_eq!(pre_output.unattributed_pre_output_ns, 0);
}

#[test]
fn pre_first_model_output_snapshot_is_absent_without_model_output() {
    let (clock, state) = timing();
    state.mark_turn_started();
    clock.set_ms(5);
    state.mark_model_request_dispatched();
    clock.set_ms(10);
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::Created),
        None
    );

    assert_eq!(
        state.complete_snapshot().profile.pre_first_model_output,
        None
    );
}

#[test]
fn turn_timing_state_records_ttfm_independently_of_visible_output() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(5);
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("hi".to_string())),
        Some(Duration::from_millis(5))
    );
    clock.set_ms(12);
    assert_eq!(
        state.record_ttfm_for_turn_item(&TurnItem::AgentMessage(AgentMessageItem {
            id: "msg-1".to_string(),
            content: Vec::new(),
            phase: None,
            memory_citation: None,
        })),
        Some(Duration::from_millis(12))
    );
    clock.set_ms(20);
    assert_eq!(
        state.record_ttfm_for_turn_item(&TurnItem::AgentMessage(AgentMessageItem {
            id: "msg-2".to_string(),
            content: Vec::new(),
            phase: None,
            memory_citation: None,
        })),
        None
    );
}

#[tokio::test]
async fn turn_timing_state_uses_one_wall_and_monotonic_start_sample() {
    let clock = Arc::new(FakeClock::new(10 * NS_PER_MS, 123_456));
    let state = TurnTimingState::with_clock(clock.clone());

    assert_eq!(state.mark_turn_started(), 123_456);
    assert_eq!(state.started_at_unix_secs().await, Some(123));

    clock.set(35 * NS_PER_MS, 987_654);
    let snapshot = state.complete_snapshot();
    assert_eq!(snapshot.duration_ms, Some(25));
    assert_eq!(snapshot.completed_at_unix_secs, Some(987));
}

#[test]
fn tool_calls_are_model_output_but_not_visible_output() {
    let function_call = ResponseItem::FunctionCall {
        id: None,
        name: "shell".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(response_item_records_model_output(&function_call));
    assert!(!response_item_records_visible_output(&function_call));

    let visible_message = ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: "hello".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    assert!(response_item_records_model_output(&visible_message));
    assert!(response_item_records_visible_output(&visible_message));
}

#[test]
fn decision_latency_excludes_reasoning_and_partial_tool_arguments() {
    assert!(!response_event_records_actionable_output(
        &ResponseEvent::ReasoningContentDelta {
            delta: "thinking".to_string(),
            content_index: 0,
        }
    ));
    assert!(!response_event_records_actionable_output(
        &ResponseEvent::ToolCallInputDelta {
            item_id: "item-1".to_string(),
            call_id: Some("call-1".to_string()),
            delta: "{\"path\":".to_string(),
        }
    ));
    assert!(response_event_records_actionable_output(
        &ResponseEvent::OutputTextDelta("answer".to_string())
    ));
    assert!(response_event_records_actionable_output(
        &ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            id: None,
            name: "shell".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "call-1".to_string(),
            internal_chat_message_metadata_passthrough: None,
        })
    ));
}

#[test]
fn first_useful_action_and_first_model_output_are_distinct_milestones() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(2);
    state.record_user_input();

    clock.set_ms(5);
    let function_call = ResponseItem::FunctionCall {
        id: None,
        name: "shell".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::OutputItemAdded(function_call)),
        None
    );

    clock.set_ms(8);
    state.record_tool_call("update_plan");
    clock.set_ms(9);
    state.record_tool_gate_admitted("update_plan");
    clock.set_ms(10);
    state.record_tool_handler_entry("update_plan");
    clock.set_ms(12);
    state.record_tool_call("shell");
    clock.set_ms(14);
    state.record_tool_gate_admitted("shell");
    clock.set_ms(16);
    state.record_tool_handler_entry("shell");
    clock.set_ms(18);
    state.record_tool_completion("shell", true);
    clock.set_ms(20);
    assert_eq!(
        state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("done".to_string())),
        Some(Duration::from_millis(20))
    );

    let snapshot = state.complete_snapshot();
    assert_eq!(snapshot.time_to_first_token_ms, Some(5));
    let timing = snapshot.protocol_timing();
    assert_eq!(timing.milestones.user_input_recorded_ms, Some(2));
    assert_eq!(timing.milestones.first_tool_accepted_ms, Some(8));
    assert_eq!(timing.milestones.first_tool_gate_admitted_ms, Some(9));
    assert_eq!(timing.milestones.first_tool_handler_entry_ms, Some(10));
    assert_eq!(timing.milestones.first_useful_tool_accepted_ms, Some(12));
    assert_eq!(
        timing.milestones.first_useful_tool_gate_admitted_ms,
        Some(14)
    );
    assert_eq!(timing.milestones.first_model_output_ms, Some(5));
    assert_eq!(timing.milestones.first_actionable_output_ms, Some(20));
    assert_eq!(timing.milestones.first_useful_action_ms, Some(16));
    assert_eq!(
        timing.milestones.first_successful_useful_action_ms,
        Some(18)
    );
    assert_eq!(timing.milestones.first_visible_output_ms, Some(20));
    assert_eq!(timing.counters.tool_call_count, 2);
}

#[test]
fn delivered_tool_relay_timing_persists_every_lifecycle_boundary() {
    let (clock, state) = timing();
    state.mark_turn_started();
    clock.set_ms(100);
    state.record_tool_dispatch_timing(
        "call-1",
        "shell_command",
        TurnTimingToolCallSource::CodeMode,
        ToolDispatchTimingSnapshot {
            lifecycle_events: vec![
                lifecycle_event(ToolLifecycleBoundary::RequestCreated, 20),
                lifecycle_event(ToolLifecycleBoundary::Admitted, 35),
                lifecycle_event(ToolLifecycleBoundary::HandlerStart, 40),
                lifecycle_event(ToolLifecycleBoundary::ProcessSpawn, 42),
                lifecycle_event(ToolLifecycleBoundary::ProcessExit, 62),
                lifecycle_event(ToolLifecycleBoundary::HandlerReturn, 70),
                lifecycle_event(ToolLifecycleBoundary::RelayEnqueue, 75),
                lifecycle_event(ToolLifecycleBoundary::RelayDelivery, 100),
            ],
            outcome: Some("failure"),
            item_to_first_poll_ms: Some(10),
            parallel_gate_wait_ms: Some(5),
            authorization_state_coordination_ms: Some(2),
            first_poll_to_handler_entry_ms: Some(10),
            handler_duration_ms: Some(30),
            workspace_evidence_before_ms: Some(1),
            workspace_evidence_after_ms: Some(2),
            pre_tool_hook_ms: Some(3),
            post_tool_hook_ms: Some(4),
            output_projection_ms: Some(5),
            history_persistence_ms: Some(6),
            first_poll_to_output_collected_ms: Some(45),
            exec_request_to_spawn_ms: Some(22),
            exec_spawn_to_exit_ms: Some(20),
            exec_exit_to_delivery_ms: Some(38),
            exec_spawn_to_delivery_ms: Some(58),
            exec_process_alive_at_delivery: false,
            exec_cleanup_state_observed: true,
            exec_background_process_expected: false,
            exec_running_process_after_cleanup: true,
            post_handler_ms: Some(30),
            total_duration_ms: Some(70),
            parallel_gate_admitted: true,
            eager: true,
            ..ToolDispatchTimingSnapshot::default()
        },
    );
    clock.set_ms(120);
    let mut pending = Some(ContinuationCause::ToolResult);
    state.begin_model_generation(&mut pending, &SessionSource::Cli);

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.tool_call_timing_overflow, 0);
    assert_eq!(timing.tool_calls.len(), 1);
    let call = &timing.tool_calls[0];
    assert_eq!(call.call_id, "call-1");
    assert_eq!(call.source, TurnTimingToolCallSource::CodeMode);
    assert_eq!(call.outcome.as_deref(), Some("failure"));
    assert_eq!(call.accepted_at_ms, Some(20));
    assert_eq!(call.first_poll_at_ms, Some(30));
    assert_eq!(call.parallel_gate_admitted_at_ms, Some(35));
    assert_eq!(call.handler_entry_at_ms, Some(40));
    assert_eq!(call.handler_exit_at_ms, Some(70));
    assert_eq!(call.output_collected_at_ms, Some(75));
    assert_eq!(call.process_spawned_at_ms, Some(42));
    assert_eq!(call.process_exited_at_ms, Some(62));
    assert_eq!(call.delivered_at_ms, Some(100));
    assert_eq!(call.model_resumed_at_ms, Some(120));
    assert_eq!(call.post_handler_ms, Some(30));
    assert!(call.eager);
    assert!(call.exec_cleanup_state_observed);
    assert!(!call.background_process_expected);
    assert!(call.running_process_after_cleanup);
}

fn lifecycle_event(boundary: ToolLifecycleBoundary, at_ms: u64) -> TurnTimingToolLifecycleEvent {
    TurnTimingToolLifecycleEvent {
        boundary,
        at_ms,
        context: ToolLifecycleContext::default(),
        retry_count: 0,
        reentry_count: 0,
    }
}

#[test]
fn lifecycle_serialization_counters_separate_emission_polling_and_admission_peak() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    let sampling = state.begin_sampling();
    let mut pending = None;
    state.begin_model_generation(&mut pending, &SessionSource::Cli);
    drop(state.begin_model_request_wait());
    drop(sampling);

    state.record_model_emitted_tool_call();
    state.record_model_emitted_tool_call();
    state.record_tool_call("exec_command");
    state.record_tool_call("exec_command");
    state.record_model_tool_gate_admitted();
    state.record_model_tool_gate_admitted();
    state.record_model_tool_gate_released();
    state.record_model_tool_gate_released();

    let timing = state.complete_snapshot().protocol_timing();
    let request = timing
        .model_requests
        .iter()
        .find(|request| request.model_emitted_tool_call_count > 0)
        .expect("primary request with model tool emissions");
    assert_eq!(request.model_emitted_tool_call_count, 2);
    assert_eq!(request.tool_call_count, 2);
    assert_eq!(request.executor_admitted_tool_call_count, 2);
    assert_eq!(request.executor_max_concurrent_tool_calls, 2);
}

#[test]
fn background_process_exit_updates_a_tool_already_delivered_alive() {
    let (clock, state) = timing();
    state.mark_turn_started();
    clock.set_ms(40);
    state.record_tool_dispatch_timing(
        "call-background",
        "exec_command",
        TurnTimingToolCallSource::Direct,
        ToolDispatchTimingSnapshot {
            item_to_first_poll_ms: Some(2),
            exec_request_to_spawn_ms: Some(5),
            exec_spawn_to_delivery_ms: Some(20),
            exec_process_alive_at_delivery: true,
            total_duration_ms: Some(30),
            ..ToolDispatchTimingSnapshot::default()
        },
    );
    state.record_background_tool_process_exit(
        "call-background",
        ToolDispatchTimingSnapshot {
            exec_spawn_to_exit_ms: Some(75),
            ..ToolDispatchTimingSnapshot::default()
        },
    );

    let timing = state.complete_snapshot().protocol_timing();
    let call = &timing.tool_calls[0];
    assert_eq!(call.process_spawned_at_ms, Some(13));
    assert_eq!(call.process_exited_at_ms, Some(88));
    assert!(call.process_alive_at_delivery);
}

#[test]
fn background_process_exit_racing_delivery_is_applied_when_tool_is_recorded() {
    let (clock, state) = timing();
    state.mark_turn_started();
    state.record_background_tool_process_exit(
        "call-racing",
        ToolDispatchTimingSnapshot {
            exec_spawn_to_exit_ms: Some(12),
            ..ToolDispatchTimingSnapshot::default()
        },
    );
    clock.set_ms(25);
    state.record_tool_dispatch_timing(
        "call-racing",
        "exec_command",
        TurnTimingToolCallSource::Direct,
        ToolDispatchTimingSnapshot {
            item_to_first_poll_ms: Some(0),
            exec_request_to_spawn_ms: Some(3),
            total_duration_ms: Some(20),
            ..ToolDispatchTimingSnapshot::default()
        },
    );

    let timing = state.complete_snapshot().protocol_timing();
    let call = &timing.tool_calls[0];
    assert_eq!(call.process_spawned_at_ms, Some(8));
    assert_eq!(call.process_exited_at_ms, Some(20));
}

#[test]
fn accepted_or_gate_admitted_tool_without_handler_entry_is_not_useful() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(3);
    state.record_tool_call("shell");
    clock.set_ms(7);
    state.record_tool_gate_admitted("shell");
    clock.set_ms(9);
    state.record_tool_completion("shell", false);

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.milestones.first_tool_accepted_ms, Some(3));
    assert_eq!(timing.milestones.first_tool_gate_admitted_ms, Some(7));
    assert_eq!(timing.milestones.first_useful_action_ms, None);
    assert_eq!(timing.milestones.first_successful_useful_action_ms, None);
}

#[test]
fn empty_and_tool_output_items_do_not_record_visible_output() {
    assert!(!response_item_records_visible_output(
        &ResponseItem::Message {
            id: None,
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: String::new(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
    ));
    assert!(!response_item_records_model_output(
        &ResponseItem::FunctionCallOutput {
            id: None,
            call_id: "call-1".to_string(),
            output: FunctionCallOutputPayload::from_text("ok".to_string()),
            internal_chat_message_metadata_passthrough: None,
        }
    ));
}

#[test]
fn legacy_profile_projection_preserves_existing_phase_breakdown() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(100);
    let sampling = state.begin_sampling();
    clock.set_ms(600);
    drop(sampling);
    let tool = state.begin_tool_blocking();
    clock.set_ms(900);
    drop(tool);
    state.record_sampling_retry();
    clock.set_ms(1_000);
    let sampling = state.begin_sampling();
    clock.set_ms(1_200);
    drop(sampling);
    clock.set_ms(1_300);

    assert_eq!(
        state.complete_snapshot().legacy_profile,
        TurnProfile {
            before_first_sampling_ms: 100,
            sampling_ms: 700,
            between_sampling_overhead_ms: 100,
            tool_blocking_ms: 300,
            after_last_sampling_ms: 100,
            sampling_request_count: 2,
            sampling_retry_count: 1,
            compaction: 0,
            tool_result: 0,
            server_end_turn_false: 0,
            pending_input: 0,
            stop_hook: 0,
            completion_review_repair: 0,
            invalid_image_recovery: 0,
        }
    );
}

#[test]
fn continuation_counters_are_consumed_once_and_retries_are_not_recounted() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    drop(state.begin_sampling());

    let causes = [
        ContinuationCause::Compaction,
        ContinuationCause::ToolResult,
        ContinuationCause::ServerEndTurnFalse,
        ContinuationCause::PendingInput,
        ContinuationCause::StopHook,
        ContinuationCause::CompletionReviewRepair,
        ContinuationCause::InvalidImageRecovery,
    ];
    for cause in causes {
        let mut pending = Some(cause);
        let sampling = state.begin_sampling();
        state.begin_model_generation(&mut pending, &SessionSource::Cli);
        drop(sampling);
        assert_eq!(pending, None);
    }
    state.record_sampling_retry();
    drop(state.begin_sampling());

    let profile = state.complete_snapshot().legacy_profile;
    assert_eq!(profile.compaction, 1);
    assert_eq!(profile.tool_result, 1);
    assert_eq!(profile.server_end_turn_false, 1);
    assert_eq!(profile.pending_input, 1);
    assert_eq!(profile.stop_hook, 1);
    assert_eq!(profile.completion_review_repair, 1);
    assert_eq!(profile.invalid_image_recovery, 1);
    let continuation_sum = profile.compaction
        + profile.tool_result
        + profile.server_end_turn_false
        + profile.pending_input
        + profile.stop_hook
        + profile.completion_review_repair
        + profile.invalid_image_recovery;
    assert_eq!(
        continuation_sum + profile.sampling_retry_count,
        profile.sampling_request_count.saturating_sub(1)
    );
}

#[test]
fn decision_latency_records_dispatch_actionable_output_and_completion() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(10);
    let initial_wait = state.begin_model_request_wait();
    clock.set_ms(20);
    state.mark_model_request_dispatched();
    clock.set_ms(25);
    state.record_response_event_milestones(&ResponseEvent::ReasoningContentDelta {
        delta: "thinking".to_string(),
        content_index: 0,
    });
    clock.set_ms(30);
    drop(initial_wait);
    state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("first".to_string()));
    clock.set_ms(40);
    state.record_response_event_milestones(&ResponseEvent::Completed {
        response_id: "response-1".to_string(),
        token_usage: None,
        end_turn: Some(false),
    });

    clock.set_ms(50);
    let continuation_wait = state.begin_model_request_wait();
    clock.set_ms(60);
    state.mark_model_request_dispatched();
    clock.set_ms(65);
    drop(continuation_wait);
    clock.set_ms(70);
    state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("second".to_string()));
    clock.set_ms(80);
    state.record_response_event_milestones(&ResponseEvent::Completed {
        response_id: "response-2".to_string(),
        token_usage: None,
        end_turn: Some(true),
    });

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.schema_version, 24);
    assert_eq!(timing.model_requests.len(), 2);
    assert_eq!(timing.model_requests[0].dispatch_ms, Some(20));
    assert_eq!(timing.model_requests[0].first_model_output_ms, Some(25));
    assert_eq!(
        timing.model_requests[0].first_actionable_output_ms,
        Some(30)
    );
    assert_eq!(
        timing.model_requests[0].decision_latency_ns,
        Some(10 * NS_PER_MS as u64)
    );
    assert_eq!(timing.model_requests[0].completed_ms, Some(40));
    assert!(!timing.model_requests[0].is_continuation);
    assert_eq!(timing.model_requests[1].dispatch_ms, Some(60));
    assert_eq!(timing.model_requests[1].first_model_output_ms, Some(70));
    assert_eq!(
        timing.model_requests[1].first_actionable_output_ms,
        Some(70)
    );
    assert_eq!(timing.model_requests[1].completed_ms, Some(80));
    assert!(timing.model_requests[1].is_continuation);
}

#[test]
fn decision_latency_correlates_logical_and_physical_attempt_identities() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    drop(state.begin_model_request_wait());

    state.record_model_attempt_identity("sampling-1", "attempt-1");
    state.record_model_attempt_identity("sampling-1", "attempt-2");
    state.record_model_attempt_identity("sampling-1", "attempt-2");

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(
        timing.model_requests[0].sampling_request_id.as_deref(),
        Some("sampling-1")
    );
    assert_eq!(
        timing.model_requests[0].physical_attempt_ids,
        vec!["attempt-1".to_string(), "attempt-2".to_string()]
    );
}

#[test]
fn decision_latency_unions_parallel_tool_time_per_generation() {
    let (clock, state) = timing();
    state.mark_turn_started();
    let mut pending = None;
    state.begin_model_generation_with_metadata(
        &mut pending,
        &SessionSource::Cli,
        Some(TurnTimingGenerationPurpose::ImplementationDecision),
        TurnTimingGenerationDisposition::DecisionBearing,
        None,
    );
    drop(state.begin_model_request_wait());

    clock.set_ms(10);
    let first = state.begin_tool_execution();
    clock.set_ms(20);
    let second = state.begin_tool_execution();
    clock.set_ms(40);
    drop(first);
    clock.set_ms(60);
    drop(second);

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.unions.tool_active_union_ns, 50 * NS_PER_MS as u64);
    assert_eq!(
        timing.model_requests[0].tool_active_union_ns,
        50 * NS_PER_MS as u64
    );
    let aggregate = timing
        .counters
        .purpose_aggregates
        .iter()
        .find(|aggregate| aggregate.purpose == TurnTimingGenerationPurpose::ImplementationDecision)
        .expect("implementation aggregate");
    assert_eq!(aggregate.tool_active_union_ns, 50 * NS_PER_MS as u64);
}

#[test]
fn request_categories_reconcile_full_logical_prompt_with_provider_usage() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    drop(state.begin_model_request_wait());
    state.record_model_request_token_categories(
        codex_protocol::protocol::TurnTimingRequestTokenCategories {
            base_instructions: 10,
            tool_schemas: 20,
            conversation_history: 30,
            logical_total: 60,
            local_input_estimate: 63,
            local_reconciliation_residual: 3,
            ..Default::default()
        },
    );
    state.record_generation_token_usage(Some(&TokenUsage {
        input_tokens: 100,
        cached_input_tokens: 40,
        output_tokens: 5,
        reasoning_output_tokens: 2,
        total_tokens: 105,
    }));

    let timing = state.complete_snapshot().protocol_timing();
    let categories = timing.model_requests[0]
        .request_token_categories
        .as_ref()
        .expect("request categories");
    assert_eq!(categories.logical_total, 60);
    assert_eq!(categories.provider_input_tokens, Some(100));
    assert_eq!(categories.provider_reconciliation_residual, Some(40));
}

#[test]
fn typed_deterministic_generation_records_exact_disposition_and_nonprogress() {
    let (clock, state) = timing();
    state.mark_turn_started();
    let mut pending = None;
    state.begin_model_generation_with_metadata(
        &mut pending,
        &SessionSource::Cli,
        Some(TurnTimingGenerationPurpose::Wait),
        TurnTimingGenerationDisposition::Deterministic,
        Some("trusted-state".to_string()),
    );
    let request_wait = state.begin_model_request_wait();
    state.record_model_attempt_identity("sampling-1", "attempt-1");
    state.record_model_attempt_identity("sampling-1", "attempt-2");
    clock.set_ms(5);
    state.mark_model_request_dispatched();
    drop(request_wait);
    let stream_wait = state.begin_model_stream_wait();
    clock.set_ms(20);
    state.record_response_event_milestones(&ResponseEvent::OutputTextDelta("done".to_string()));
    clock.set_ms(25);
    drop(stream_wait);
    state.record_generation_token_usage(Some(&TokenUsage {
        input_tokens: 100,
        cached_input_tokens: 20,
        output_tokens: 10,
        reasoning_output_tokens: 4,
        total_tokens: 110,
    }));
    state.record_generation_outcome(Vec::new(), false, true);
    let timing = state.complete_snapshot().protocol_timing();
    let usage = timing.model_requests[0]
        .token_usage
        .as_ref()
        .expect("physical usage");
    assert_eq!(usage.input_tokens, 100);
    assert_eq!(usage.cached_input_tokens, 20);
    assert_eq!(usage.visible_output_tokens, 6);
    assert_eq!(usage.reasoning_tokens, 4);
    assert_eq!(usage.total_tokens, 110);
    assert_eq!(
        timing.model_requests[0].disposition,
        TurnTimingGenerationDisposition::Deterministic
    );
    assert!(timing.model_requests[0].unchanged_relevant_state);
    assert!(!timing.model_requests[0].next_structured_action_changed);

    assert_eq!(timing.counters.generations_by_disposition.deterministic, 1);
    assert_eq!(
        timing.counters.generations_by_disposition.decision_bearing,
        0
    );
    assert_eq!(timing.counters.generations_by_disposition.unknown, 0);

    assert_eq!(
        timing.observational_nonprogress_tokens.logical_generations,
        1
    );
    assert_eq!(timing.observational_nonprogress_tokens.input_tokens, 100);
    assert_eq!(
        timing.observational_nonprogress_tokens.cached_input_tokens,
        20
    );
    assert_eq!(
        timing
            .observational_nonprogress_tokens
            .visible_output_tokens,
        6
    );
    assert_eq!(timing.observational_nonprogress_tokens.reasoning_tokens, 4);
    assert_eq!(timing.observational_nonprogress_tokens.total_tokens, 110);
    assert_eq!(
        timing.observational_nonprogress_latency.logical_generations,
        1
    );
    assert_eq!(
        timing.observational_nonprogress_latency.physical_attempts,
        2
    );
    assert_eq!(
        timing
            .observational_nonprogress_latency
            .model_stream_wait_ns,
        20 * NS_PER_MS as u64
    );
    assert_eq!(
        timing
            .observational_nonprogress_latency
            .decision_ready_attempts,
        1
    );
    assert_eq!(
        timing.observational_nonprogress_latency.decision_latency_ns,
        15 * NS_PER_MS as u64
    );
}

#[test]
fn typed_generation_dispositions_drive_exact_started_counts() {
    let (_clock, state) = timing();
    state.mark_turn_started();

    for disposition in [
        TurnTimingGenerationDisposition::Unknown,
        TurnTimingGenerationDisposition::DecisionBearing,
        TurnTimingGenerationDisposition::Deterministic,
    ] {
        let mut pending = None;
        state.begin_model_generation_with_metadata(
            &mut pending,
            &SessionSource::Cli,
            Some(TurnTimingGenerationPurpose::InitialReasoning),
            disposition,
            None,
        );
        drop(state.begin_model_request_wait());
    }

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.disposition)
            .collect::<Vec<_>>(),
        vec![
            TurnTimingGenerationDisposition::Unknown,
            TurnTimingGenerationDisposition::DecisionBearing,
            TurnTimingGenerationDisposition::Deterministic,
        ]
    );
    assert_eq!(timing.counters.generations_by_disposition.unknown, 1);
    assert_eq!(
        timing.counters.generations_by_disposition.decision_bearing,
        1
    );
    assert_eq!(timing.counters.generations_by_disposition.deterministic, 1);
    assert_eq!(
        timing.counters.suppressed_deterministic_continuation_count,
        0
    );
}

#[test]
fn accepted_batched_receipt_counts_suppressed_boundaries_without_starting_generations() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    let receipt = TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::UnchangedWait,
        wire_identity: String::new(),
        resource_identity_hash: "batched-resource".to_string(),
        state_revision: "revision".to_string(),
        host_action: DeterministicContinuationHostAction::AwaitStateChange,
        action_bounds_hash: "batched-bounds".to_string(),
        suppressed_continuation_count: 7,
    };
    state.record_accepted_deterministic_continuation_receipts(&[receipt]);
    state.record_accepted_deterministic_continuation_receipts(&[
        TurnTimingDeterministicContinuationReceipt {
            suppressed_continuation_count: 0,
            resource_identity_hash: "zero-resource".to_string(),
            state_revision: "revision".to_string(),
            action_bounds_hash: "zero-bounds".to_string(),
            ..Default::default()
        },
        TurnTimingDeterministicContinuationReceipt {
            suppressed_continuation_count: 3,
            ..Default::default()
        },
    ]);

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.suppressed_deterministic_continuation_count, 7);
    assert_eq!(counters.logical_generation_count, 0);
    assert_eq!(counters.generations_by_disposition, Default::default());
}

#[test]
fn internally_drained_agent_event_pages_do_not_start_model_generations() {
    let (_clock, state) = timing();
    state.mark_turn_started();

    state.record_internally_drained_waits(5);

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.internally_drained_wait_count, 5);
    assert_eq!(counters.logical_generation_count, 0);
    assert_eq!(counters.generations_by_disposition, Default::default());
}

#[test]
fn continuation_receipts_aggregate_saturate_and_bound_distinct_groups() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    let receipt = TurnTimingDeterministicContinuationReceipt {
        class: DeterministicContinuationClass::UnchangedWait,
        wire_identity: String::new(),
        resource_identity_hash: "hashed-resource".to_string(),
        state_revision: "revision".to_string(),
        host_action: DeterministicContinuationHostAction::AwaitStateChange,
        action_bounds_hash: "bounds".to_string(),
        suppressed_continuation_count: u32::MAX,
    };
    state.record_accepted_deterministic_continuation_receipts(std::slice::from_ref(&receipt));
    let mut additional = receipt;
    additional.suppressed_continuation_count = 1;
    state.record_accepted_deterministic_continuation_receipts(&[additional]);

    for index in 0..64 {
        state.record_accepted_deterministic_continuation_receipts(&[
            TurnTimingDeterministicContinuationReceipt {
                class: DeterministicContinuationClass::ArtifactRange,
                wire_identity: String::new(),
                resource_identity_hash: format!("resource-{index}"),
                state_revision: "revision".to_string(),
                host_action: DeterministicContinuationHostAction::DrainArtifactRanges,
                action_bounds_hash: format!("bounds-{index}"),
                suppressed_continuation_count: 1,
            },
        ]);
    }

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.deterministic_continuation_receipts.len(), 64);
    assert_eq!(timing.deterministic_continuation_receipt_overflow, 1);
    assert_eq!(
        timing.counters.suppressed_deterministic_continuation_count,
        u32::MAX
    );
    assert_eq!(
        timing.counters.generations_by_disposition,
        Default::default()
    );
    let aggregated = timing
        .deterministic_continuation_receipts
        .iter()
        .find(|candidate| candidate.resource_identity_hash == "hashed-resource")
        .expect("aggregated receipt");
    assert_eq!(aggregated.suppressed_continuation_count, u32::MAX);
}

#[test]
fn controlled_final_proof_fixture_reports_request_and_token_reduction() {
    fn record_request(
        state: &Arc<TurnTimingState>,
        purpose: TurnTimingGenerationPurpose,
        usage: TokenUsage,
    ) {
        let mut pending = None;
        state.begin_model_generation_with_metadata(
            &mut pending,
            &SessionSource::Cli,
            Some(purpose),
            TurnTimingGenerationDisposition::DecisionBearing,
            Some(format!("final-proof-{purpose:?}")),
        );
        drop(state.begin_model_request_wait());
        state.record_generation_token_usage(Some(&usage));
        state.record_generation_outcome(Vec::new(), true, false);
    }

    let (_clock, legacy) = timing();
    legacy.mark_turn_started();
    record_request(
        &legacy,
        TurnTimingGenerationPurpose::ImplementationDecision,
        TokenUsage {
            input_tokens: 300,
            cached_input_tokens: 100,
            output_tokens: 30,
            reasoning_output_tokens: 15,
            total_tokens: 330,
        },
    );
    record_request(
        &legacy,
        TurnTimingGenerationPurpose::ValidationInterpretation,
        TokenUsage {
            input_tokens: 340,
            cached_input_tokens: 120,
            output_tokens: 32,
            reasoning_output_tokens: 16,
            total_tokens: 372,
        },
    );
    record_request(
        &legacy,
        TurnTimingGenerationPurpose::TerminalCompletionReasoning,
        TokenUsage {
            input_tokens: 500,
            cached_input_tokens: 180,
            output_tokens: 40,
            reasoning_output_tokens: 20,
            total_tokens: 540,
        },
    );
    let legacy = legacy.complete_snapshot().protocol_timing();

    let (_clock, final_proof) = timing();
    final_proof.mark_turn_started();
    record_request(
        &final_proof,
        TurnTimingGenerationPurpose::TerminalCompletionReasoning,
        TokenUsage {
            input_tokens: 220,
            cached_input_tokens: 80,
            output_tokens: 36,
            reasoning_output_tokens: 18,
            total_tokens: 256,
        },
    );
    let final_proof = final_proof.complete_snapshot().protocol_timing();

    assert_eq!(legacy.counters.model_request_count, 3);
    assert_eq!(final_proof.counters.model_request_count, 1);
    assert_eq!(
        legacy
            .model_requests
            .iter()
            .filter_map(|request| request.token_usage.as_ref())
            .map(|usage| usage.total_tokens)
            .sum::<u64>(),
        1_242
    );
    assert_eq!(
        final_proof
            .model_requests
            .iter()
            .filter_map(|request| request.token_usage.as_ref())
            .map(|usage| usage.total_tokens)
            .sum::<u64>(),
        256
    );
    let finalization = final_proof
        .counters
        .purpose_aggregates
        .iter()
        .find(|aggregate| {
            aggregate.purpose == TurnTimingGenerationPurpose::TerminalCompletionReasoning
        })
        .expect("completion finalization aggregate");
    assert_eq!(finalization.input_tokens, 220);
    assert_eq!(finalization.cached_input_tokens, 80);
    assert_eq!(finalization.visible_output_tokens, 18);
    assert_eq!(finalization.reasoning_output_tokens, 18);
    assert_eq!(finalization.total_tokens, 256);
}

#[test]
fn validation_failure_diagnosis_repair_and_rereview_remain_decision_bearing() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    for (index, purpose) in [
        TurnTimingGenerationPurpose::ImplementationDecision,
        TurnTimingGenerationPurpose::FailureDiagnosis,
        TurnTimingGenerationPurpose::Repair,
        TurnTimingGenerationPurpose::ValidationInterpretation,
    ]
    .into_iter()
    .enumerate()
    {
        let mut pending = (index > 0).then_some(if index == 2 {
            ContinuationCause::CompletionReviewRepair
        } else {
            ContinuationCause::ToolResult
        });
        state.begin_model_generation_with_metadata(
            &mut pending,
            &SessionSource::Cli,
            Some(purpose),
            TurnTimingGenerationDisposition::DecisionBearing,
            Some(format!("decision-state-{index}")),
        );
        drop(state.begin_model_request_wait());
        state.record_generation_outcome(
            vec![if index == 2 {
                TurnTimingProgressKind::WorkspaceMutation
            } else {
                TurnTimingProgressKind::ValidationResult
            }],
            true,
            false,
        );
    }

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.counters.logical_generation_count, 4);
    assert_eq!(timing.model_requests.len(), 4);
    assert!(timing.model_requests.iter().all(|request| {
        request.disposition == TurnTimingGenerationDisposition::DecisionBearing
    }));
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.generation_purpose)
            .collect::<Vec<_>>(),
        vec![
            Some(TurnTimingGenerationPurpose::ImplementationDecision),
            Some(TurnTimingGenerationPurpose::FailureDiagnosis),
            Some(TurnTimingGenerationPurpose::Repair),
            Some(TurnTimingGenerationPurpose::ValidationInterpretation),
        ]
    );
}

#[test]
fn logical_generations_are_classified_by_workflow_purpose() {
    let (_clock, state) = timing();
    state.mark_turn_started();

    let cases = [
        (None, SessionSource::Cli),
        (Some(ContinuationCause::ToolResult), SessionSource::Cli),
        (
            Some(ContinuationCause::CompletionReviewRepair),
            SessionSource::Cli,
        ),
        (None, SessionSource::SubAgent(SubAgentSource::Review)),
        (
            None,
            SessionSource::SubAgent(SubAgentSource::Other("test".to_string())),
        ),
        (Some(ContinuationCause::PendingInput), SessionSource::Cli),
    ];

    for (mut pending, source) in cases {
        state.begin_model_generation(&mut pending, &source);
        drop(state.begin_model_request_wait());
    }
    state.begin_compaction_generation();
    drop(state.begin_model_request_wait());

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.counters.logical_generation_count, 7);
    assert_eq!(timing.counters.generations_by_reason.initial, 1);
    assert_eq!(timing.counters.generations_by_reason.tool_continuation, 1);
    assert_eq!(timing.counters.generations_by_reason.completion_review, 1);
    assert_eq!(
        timing
            .counters
            .generations_by_reason
            .completion_repair_rereview,
        1
    );
    assert_eq!(timing.counters.generations_by_reason.compaction, 1);
    assert_eq!(timing.counters.generations_by_reason.subagent, 1);
    assert_eq!(timing.counters.generations_by_reason.other, 1);
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.generation_reason)
            .collect::<Vec<_>>(),
        vec![
            TurnTimingGenerationReason::Initial,
            TurnTimingGenerationReason::ToolContinuation,
            TurnTimingGenerationReason::CompletionRepairRereview,
            TurnTimingGenerationReason::CompletionReview,
            TurnTimingGenerationReason::Subagent,
            TurnTimingGenerationReason::Other,
            TurnTimingGenerationReason::Compaction,
        ]
    );
}

#[test]
fn deterministic_primary_retry_and_fallback_attempts_reconcile_without_inflating_generations() {
    let (clock, state) = timing();
    state.mark_turn_started();
    let mut pending = None;
    state.begin_model_generation_with_metadata(
        &mut pending,
        &SessionSource::Cli,
        Some(TurnTimingGenerationPurpose::TerminalCompletionReasoning),
        TurnTimingGenerationDisposition::Deterministic,
        Some("terminal-state".to_string()),
    );

    drop(state.begin_model_request_wait());
    let primary = state.begin_model_stream_wait();
    clock.set_ms(10);
    drop(primary);
    state.record_tool_call("shell");
    state.record_tool_call("shell");

    state.record_model_retry();
    drop(state.begin_model_request_wait());
    let retry = state.begin_model_stream_wait();
    clock.set_ms(15);
    drop(retry);

    state.record_model_fallback();
    state.record_model_retry();
    drop(state.begin_model_request_wait());
    let fallback = state.begin_model_stream_wait();
    clock.set_ms(22);
    drop(fallback);

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.counters.logical_generation_count, 1);
    assert_eq!(timing.counters.generations_by_disposition.deterministic, 1);
    assert_eq!(timing.counters.model_request_count, 3);
    assert_eq!(timing.counters.attempts_by_kind.primary, 1);
    assert_eq!(timing.counters.attempts_by_kind.retry, 1);
    assert_eq!(timing.counters.attempts_by_kind.fallback, 1);
    assert_eq!(
        timing.counters.model_request_count,
        timing.counters.attempts_by_kind.primary
            + timing.counters.attempts_by_kind.retry
            + timing.counters.attempts_by_kind.fallback
    );
    assert_eq!(
        timing
            .model_requests
            .iter()
            .map(|request| request.attempt_kind)
            .collect::<Vec<_>>(),
        vec![
            TurnTimingAttemptKind::Primary,
            TurnTimingAttemptKind::Retry,
            TurnTimingAttemptKind::Fallback,
        ]
    );
    assert_eq!(
        timing.model_requests[0].model_stream_wait_ns,
        10 * NS_PER_MS as u64
    );
    assert_eq!(
        timing.model_requests[1].model_stream_wait_ns,
        5 * NS_PER_MS as u64
    );
    assert_eq!(
        timing.model_requests[2].model_stream_wait_ns,
        7 * NS_PER_MS as u64
    );
    assert_eq!(timing.model_requests[0].tool_call_count, 2);
    assert_eq!(timing.model_requests[1].tool_call_count, 0);
    assert_eq!(timing.model_requests[2].tool_call_count, 0);
    assert!(
        timing.model_requests.iter().all(|request| {
            request.disposition == TurnTimingGenerationDisposition::Deterministic
        })
    );
}

#[test]
fn repeated_wait_uses_exact_purpose() {
    let (_clock, state) = timing();
    state.mark_turn_started();

    for fingerprint in ["wait-a", "wait-a", "wait-b"] {
        let mut pending = Some(ContinuationCause::ToolResult);
        state.begin_model_generation_with_metadata(
            &mut pending,
            &SessionSource::Cli,
            Some(TurnTimingGenerationPurpose::Wait),
            TurnTimingGenerationDisposition::DecisionBearing,
            Some(fingerprint.to_string()),
        );
        drop(state.begin_model_request_wait());
    }

    let mut pending = Some(ContinuationCause::ToolResult);
    state.begin_model_generation_with_metadata(
        &mut pending,
        &SessionSource::Cli,
        Some(TurnTimingGenerationPurpose::ImplementationDecision),
        TurnTimingGenerationDisposition::DecisionBearing,
        Some("candidate-a".to_string()),
    );
    drop(state.begin_model_request_wait());

    let mut pending = Some(ContinuationCause::CompletionReviewRepair);
    state.begin_model_generation_with_metadata(
        &mut pending,
        &SessionSource::Cli,
        Some(TurnTimingGenerationPurpose::Repair),
        TurnTimingGenerationDisposition::DecisionBearing,
        Some("repair-a".to_string()),
    );
    drop(state.begin_model_request_wait());

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.exact_repeated_wait_count, 1);
}

#[test]
fn failure_signature_count_uses_unique_failure_identities() {
    let (_clock, state) = timing();
    state.mark_turn_started();

    for (state_fingerprint, failure_fingerprint) in [
        ("state-a", "failure-a"),
        ("state-b", "failure-a"),
        ("state-b", "failure-b"),
    ] {
        let mut pending = Some(ContinuationCause::ToolResult);
        state.begin_model_generation_with_failure_metadata(
            &mut pending,
            &SessionSource::Cli,
            Some(TurnTimingGenerationPurpose::FailureDiagnosis),
            TurnTimingGenerationDisposition::DecisionBearing,
            Some(state_fingerprint.to_string()),
            Some(failure_fingerprint.to_string()),
        );
        drop(state.begin_model_request_wait());
    }

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.failure_diagnosis_count, 3);
    assert_eq!(counters.failure_signature_count, 2);
}

#[test]
fn zero_requests_and_cancellation_before_request_do_not_count_continuations() {
    let (_clock, state) = timing();
    let pending = Some(ContinuationCause::PendingInput);
    let profile = state.complete_snapshot().legacy_profile;
    assert_eq!(profile.sampling_request_count, 0);
    assert_eq!(profile.pending_input, 0);
    assert_eq!(pending, Some(ContinuationCause::PendingInput));

    state.mark_turn_started();
    let profile = state.complete_snapshot().legacy_profile;
    assert_eq!(profile.sampling_request_count, 0);
    assert_eq!(profile.pending_input, 0);
}

#[test]
fn exclusive_ledger_partitions_every_nanosecond_and_subtracts_only_interactive_only() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(10);
    let model = state.begin_model_request_wait();
    clock.set_ms(30);
    let tool = state.begin_tool_execution();
    clock.set_ms(60);
    let interactive = state.begin_interactive_wait(InteractiveWaitKind::Approval);
    clock.set_ms(70);
    drop(interactive);
    clock.set_ms(90);
    drop(tool);
    clock.set_ms(100);
    drop(model);
    let interactive = state.begin_interactive_wait(InteractiveWaitKind::Permission);
    clock.set_ms(115);
    drop(interactive);
    let retry = state.begin_retry_backoff();
    clock.set_ms(120);
    drop(retry);
    let standalone = state.begin_standalone_work();
    clock.set_ms(127);
    drop(standalone);
    state.begin_finalization();
    clock.set_ms(140);

    let profile = state.complete_snapshot().profile;
    assert_eq!(profile.schema_version, 24);
    assert!(profile.profile_valid);
    assert!(profile.classification_complete);
    assert_eq!(profile.inclusive_duration_ns, 140 * NS_PER_MS);
    assert_eq!(profile.machine_duration_ns, 125 * NS_PER_MS);
    assert_eq!(profile.exclusive.orchestration_ns, 10 * NS_PER_MS);
    assert_eq!(profile.exclusive.model_only_ns, 30 * NS_PER_MS);
    assert_eq!(profile.exclusive.model_tool_overlap_ns, 50 * NS_PER_MS);
    assert_eq!(
        profile.exclusive.interactive_machine_overlap_ns,
        10 * NS_PER_MS
    );
    assert_eq!(profile.exclusive.interactive_only_wait_ns, 15 * NS_PER_MS);
    assert_eq!(profile.exclusive.retry_only_ns, 5 * NS_PER_MS);
    assert_eq!(profile.exclusive.standalone_work_ns, 7 * NS_PER_MS);
    assert_eq!(profile.exclusive.finalization_ns, 13 * NS_PER_MS);
    assert_eq!(profile.unions.model_active_ns, 90 * NS_PER_MS);
    assert_eq!(profile.unions.tool_active_ns, 60 * NS_PER_MS);
    assert_eq!(profile.unions.interactive_wait_ns, 25 * NS_PER_MS);
}

#[test]
fn accurate_unclassified_time_does_not_invalidate_profile() {
    let (clock, state) = timing();
    state.mark_turn_started();
    let model = state.begin_model_stream_wait();
    let retry = state.begin_retry_backoff();
    clock.set_ms(20);
    drop(retry);
    drop(model);
    state.begin_finalization();
    clock.set_ms(25);

    let profile = state.complete_snapshot().profile;
    assert!(profile.profile_valid);
    assert!(!profile.classification_complete);
    assert_eq!(profile.exclusive.unclassified_ns, 20 * NS_PER_MS);
    assert_eq!(profile.exclusive.finalization_ns, 5 * NS_PER_MS);
}

#[test]
fn backward_monotonic_sample_is_clamped_and_invalidates_profile() {
    let clock = Arc::new(FakeClock::new(100 * NS_PER_MS, 100));
    let state = TurnTimingState::with_clock(clock.clone());
    state.mark_turn_started();
    clock.set(90 * NS_PER_MS, 200);

    let profile = state.complete_snapshot().profile;
    assert!(!profile.profile_valid);
    assert_eq!(profile.inclusive_duration_ns, 0);
    assert_eq!(profile.counters.clock_regression_count, 1);
}

#[test]
fn completion_snapshot_is_immutable() {
    let (clock, state) = timing();
    state.mark_turn_started();
    clock.set(10 * NS_PER_MS, 2_000);
    let first = state.complete_snapshot();
    clock.set(100 * NS_PER_MS, 9_000);
    let second = state.complete_snapshot();

    assert_eq!(first.completed_at_unix_secs, second.completed_at_unix_secs);
    assert_eq!(first.duration_ms, second.duration_ms);
    assert_eq!(first.profile, second.profile);
}

#[test]
fn duplicate_start_is_rejected_without_resetting_elapsed_time() {
    let (clock, state) = timing();
    let first_started_at = state.mark_turn_started();
    clock.set_ms(5);
    let duplicate_started_at = state.mark_turn_started();
    clock.set_ms(10);

    let snapshot = state.complete_snapshot();
    assert_eq!(duplicate_started_at, first_started_at);
    assert_eq!(snapshot.duration_ms, Some(10));
    assert_eq!(snapshot.profile.counters.invalid_transition_count, 1);
}

#[test]
fn start_after_completion_is_rejected_without_changing_frozen_snapshot() {
    let (clock, state) = timing();
    let started_at = state.mark_turn_started();
    clock.set_ms(10);
    let completed = state.complete_snapshot();
    clock.set_ms(20);

    assert_eq!(state.mark_turn_started(), started_at);
    assert_eq!(state.complete_snapshot().profile, completed.profile);
    assert_eq!(state.state().counters.invalid_transition_count, 1);
}

#[test]
fn wait_and_tool_output_counters_are_additive() {
    let (_clock, state) = timing();

    state.record_wait_only_generation();
    state.record_internally_drained_waits(7);
    state.record_residual_deterministic_generation();
    state.record_owner_drained_continuation();
    state.record_executed_validation(125, true);
    state.record_reused_validation();
    state.record_suppressed_validation_output();
    state.record_ready_startup_prewarm();
    state.record_completion_review_ready_phase();
    state.record_completion_review_terminal_phase();
    state.record_no_progress_directive();
    state.record_proven_loop_activation();
    state.record_tool_output_projection_facts(1_000, 250, 400, 100, true, false, true, 3, true);
    state.record_tool_output_projection_facts(500, 125, 200, 50, false, true, false, 1, true);
    state.record_tool_output_artifact_reread();
    state.record_tool_output_recovery(2);
    state.record_truncation_induced_continuation();

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.wait_only_generation_count, 1);
    assert_eq!(counters.internally_drained_wait_count, 7);
    assert_eq!(counters.residual_deterministic_generation_count, 1);
    assert_eq!(counters.owner_drained_continuation_count, 1);
    assert_eq!(counters.executed_validation_count, 1);
    assert_eq!(counters.reused_validation_count, 1);
    assert_eq!(counters.duplicate_validation_count, 1);
    assert_eq!(counters.forced_fresh_validation_count, 1);
    assert_eq!(counters.executed_validation_duration_ns, 125_000_000);
    assert_eq!(counters.suppressed_validation_output_count, 1);
    assert_eq!(counters.ready_startup_prewarm_count, 1);
    assert_eq!(counters.completion_review_ready_phase_count, 1);
    assert_eq!(counters.completion_review_terminal_phase_count, 1);
    assert_eq!(counters.suppressed_deterministic_continuation_count, 0);
    assert_eq!(counters.no_progress_directive_count, 1);
    assert_eq!(counters.proven_loop_activation_count, 1);
    assert_eq!(counters.tool_output_truncation_count, 1);
    assert_eq!(counters.tool_output_projected_token_count, 150);
    assert_eq!(counters.tool_output_artifact_reread_count, 1);
    assert_eq!(counters.tool_output_canonical_byte_count, 1_500);
    assert_eq!(counters.tool_output_canonical_token_count, 375);
    assert_eq!(counters.tool_output_model_byte_count, 600);
    assert_eq!(counters.tool_output_model_token_count, 150);
    assert_eq!(counters.tool_output_artifact_creation_count, 1);
    assert_eq!(counters.tool_output_artifact_reuse_count, 1);
    assert_eq!(counters.tool_output_projection_truncation_count, 1);
    assert_eq!(counters.tool_output_omitted_section_count, 4);
    assert_eq!(counters.tool_output_recovery_call_count, 1);
    assert_eq!(counters.tool_output_recovery_retruncation_count, 2);
    assert_eq!(counters.tool_output_recursive_spill_count, 0);
    assert_eq!(counters.truncation_induced_continuation_count, 1);
}

#[test]
fn optimization_activation_decision_counters_are_additive() {
    let (_clock, state) = timing();

    state.record_tool_router_reuse();
    state.record_tool_router_rebuild();
    state.record_projection_source_dependencies_reuse();
    state.record_projection_source_dependencies_fallback();

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.tool_router_reuse_count, 1);
    assert_eq!(counters.tool_router_rebuild_count, 1);
    assert_eq!(counters.projection_source_dependencies_reuse_count, 1);
    assert_eq!(counters.projection_source_dependencies_fallback_count, 1);
}

#[test]
fn reserved_recursive_spill_counter_remains_zero_in_protocol() {
    let (_clock, state) = timing();

    state.record_tool_output_recovery(2);

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(
        counters.tool_output_recursive_spill_count,
        RESERVED_TOOL_OUTPUT_RECURSIVE_SPILL_COUNT
    );
    assert_eq!(counters.tool_output_recursive_spill_count, 0);
}

#[test]
fn projected_output_counts_only_the_next_tool_result_generation_as_recovery() {
    let (_clock, state) = timing();

    let record_projection = |tokens| {
        state.record_tool_output_projection_facts(0, 0, 0, tokens, false, false, true, 0, true);
    };
    record_projection(40);
    record_projection(20);
    let mut pending = Some(ContinuationCause::ToolResult);
    state.begin_model_generation(&mut pending, &SessionSource::Cli);

    record_projection(10);
    let mut pending = Some(ContinuationCause::PendingInput);
    state.begin_model_generation(&mut pending, &SessionSource::Cli);

    record_projection(5);
    let mut pending = Some(ContinuationCause::ToolResult);
    state.begin_model_generation(&mut pending, &SessionSource::Cli);

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.tool_output_truncation_count, 4);
    assert_eq!(counters.tool_output_projected_token_count, 75);
    assert_eq!(counters.truncation_induced_continuation_count, 2);
    assert_eq!(counters.attributable_recovery_generation_count, 2);
}

#[test]
fn named_local_phases_record_union_time_without_disturbing_partition() {
    let (clock, state) = timing();
    state.mark_turn_started();
    let preparation = state.begin_local_phase(TurnLocalPhase::Preparation);

    clock.set_ms(10);
    let serialization = state.begin_local_phase(TurnLocalPhase::Serialization);
    clock.set_ms(20);
    let persistence = state.begin_local_phase(TurnLocalPhase::Persistence);
    clock.set_ms(30);
    drop(serialization);
    clock.set_ms(40);
    drop(persistence);
    clock.set_ms(50);
    drop(preparation);

    let profile = state.complete_snapshot().profile;
    assert_eq!(profile.local.preparation_ns, 50 * NS_PER_MS);
    assert_eq!(profile.local.serialization_ns, 20 * NS_PER_MS);
    assert_eq!(profile.local.persistence_ns, 20 * NS_PER_MS);
    assert_eq!(profile.exclusive.orchestration_ns, 50 * NS_PER_MS);
    assert_eq!(profile.exclusive.total_ns(), profile.inclusive_duration_ns);
}

#[test]
fn planning_reports_inclusive_exclusive_and_nested_compaction_time() {
    let (clock, state) = timing();
    state.mark_turn_started();
    let planning = state.begin_local_phase(TurnLocalPhase::Planning);
    clock.set_ms(10);
    let compaction = state.begin_local_phase(TurnLocalPhase::Compaction);
    clock.set_ms(30);
    drop(compaction);
    clock.set_ms(50);
    drop(planning);

    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(timing.local.planning_union_ns, 50 * NS_PER_MS as u64);
    assert_eq!(
        timing.local.planning_compaction_overlap_union_ns,
        20 * NS_PER_MS as u64
    );
    assert_eq!(
        timing.local.planning_exclusive_union_ns,
        30 * NS_PER_MS as u64
    );
    assert_eq!(timing.local.compaction_union_ns, 20 * NS_PER_MS as u64);
    assert_eq!(
        timing.exclusive.orchestration_ns,
        timing.inclusive_duration_ns
    );
}

#[test]
fn planning_counters_are_deterministic_and_additive() {
    let (_clock, state) = timing();
    state.record_initial_plan_generation();
    state.record_plan_revision_generation();
    state.record_planning_fixed_point_iteration();
    state.record_planning_fixed_point_iteration();
    state.record_planning_invalidation();
    state.record_planning_semantic_effect();
    state.record_planning_failure();

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.planning_generation_count, 1);
    assert_eq!(counters.plan_revision_generation_count, 1);
    assert_eq!(counters.planning_fixed_point_iteration_count, 2);
    assert_eq!(counters.planning_invalidation_count, 1);
    assert_eq!(counters.planning_semantic_effect_count, 1);
    assert_eq!(counters.planning_failure_count, 1);
}

#[test]
fn timing_histories_evict_oldest_entries_at_their_caps() {
    let (_clock, state) = timing();
    state.mark_turn_started();

    for request_index in 0..=MAX_MODEL_REQUEST_TIMINGS {
        let mut pending = None;
        state.begin_model_generation(&mut pending, &SessionSource::Cli);
        drop(state.begin_model_request_wait());
        state.record_model_attempt_identity(
            &format!("sampling-{request_index}"),
            &format!("attempt-{request_index}"),
        );
    }

    for attempt_index in 0..=MAX_MODEL_REQUEST_PHYSICAL_ATTEMPT_IDS {
        state.record_model_attempt_identity(
            &format!("sampling-{MAX_MODEL_REQUEST_TIMINGS}"),
            &format!("latest-attempt-{attempt_index}"),
        );
    }
    state.record_generation_outcome(
        vec![TurnTimingProgressKind::WorkspaceMutation; MAX_MODEL_REQUEST_PROGRESS_KINDS + 1],
        false,
        false,
    );

    let profile = state.complete_snapshot().profile;
    assert_eq!(
        profile.counters.model_request_count,
        u32::try_from(MAX_MODEL_REQUEST_TIMINGS + 1).expect("request cap fits u32")
    );
    assert_eq!(profile.model_requests.len(), MAX_MODEL_REQUEST_TIMINGS);
    assert_eq!(
        profile.model_requests[0].sampling_request_id.as_deref(),
        Some("sampling-1")
    );
    let latest = profile.model_requests.last().expect("latest request");
    assert_eq!(
        latest.physical_attempt_ids.len(),
        MAX_MODEL_REQUEST_PHYSICAL_ATTEMPT_IDS
    );
    assert_eq!(
        latest.physical_attempt_ids.first().map(String::as_str),
        Some("latest-attempt-1")
    );
    assert_eq!(
        latest.progress_kinds.len(),
        MAX_MODEL_REQUEST_PROGRESS_KINDS
    );
}
