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
use codex_protocol::protocol::TurnTiming;
use codex_protocol::protocol::TurnTimingAttemptKind;
use codex_protocol::protocol::TurnTimingDeterministicContinuationReceipt;
use codex_protocol::protocol::TurnTimingGenerationDisposition;
use codex_protocol::protocol::TurnTimingGenerationPurpose;
use codex_protocol::protocol::TurnTimingGenerationReason;
use codex_protocol::protocol::TurnTimingProgressKind;
use pretty_assertions::assert_eq;

use super::ClockSample;
use super::ContinuationCause;
use super::InteractiveWaitKind;
use super::PreEditReopenReason;
use super::SourceDiscoveryTimingEvent;
use super::TimeSample;
use super::TurnClock;
use super::TurnLocalPhase;
use super::TurnTimingState;
use super::response_item_records_model_output;
use super::response_item_records_visible_output;
use crate::ResponseEvent;

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

    let restored: TurnTiming = serde_json::from_value(legacy).expect("legacy timing");
    assert_eq!(restored.terminalization, Default::default());
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
fn model_requests_record_dispatch_output_completion_and_continuation() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(10);
    let initial_wait = state.begin_model_request_wait();
    clock.set_ms(20);
    state.mark_model_request_dispatched();
    clock.set_ms(25);
    drop(initial_wait);
    clock.set_ms(30);
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
    assert_eq!(timing.schema_version, 11);
    assert_eq!(timing.model_requests.len(), 2);
    assert_eq!(timing.model_requests[0].dispatch_ms, Some(20));
    assert_eq!(timing.model_requests[0].first_model_output_ms, Some(30));
    assert_eq!(timing.model_requests[0].completed_ms, Some(40));
    assert!(!timing.model_requests[0].is_continuation);
    assert_eq!(timing.model_requests[1].dispatch_ms, Some(60));
    assert_eq!(timing.model_requests[1].first_model_output_ms, Some(70));
    assert_eq!(timing.model_requests[1].completed_ms, Some(80));
    assert!(timing.model_requests[1].is_continuation);
}

#[test]
fn post_discovery_usage_counts_result_cells_and_only_emits_a_ratio_with_usage() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    state.record_discovery_result_cell(25);
    state.record_discovery_result_cell(15);
    drop(state.begin_model_request_wait());
    state.record_generation_token_usage(Some(&TokenUsage {
        input_tokens: 200,
        cached_input_tokens: 80,
        output_tokens: 10,
        reasoning_output_tokens: 4,
        total_tokens: 210,
    }));

    let turn_timing = state.complete_snapshot().protocol_timing();
    let observation = turn_timing.model_requests[0]
        .post_discovery
        .as_ref()
        .expect("post-discovery observation");
    assert_eq!(observation.newly_injected_discovery_result_tokens, 40);
    assert_eq!(observation.discovery_result_cells, 2);
    assert_eq!(observation.discovery_model_boundary_count, 1);
    assert_eq!(observation.total_provider_input_tokens, 200);
    assert_eq!(observation.cached_input_tokens, 80);
    assert_eq!(observation.replay_amplification_micros, Some(5_000_000));

    let (_clock, state) = timing();
    state.mark_turn_started();
    state.record_discovery_result_cell(40);
    drop(state.begin_model_request_wait());
    let timing = state.complete_snapshot().protocol_timing();
    assert_eq!(
        timing.model_requests[0]
            .post_discovery
            .as_ref()
            .expect("post-discovery observation")
            .replay_amplification_micros,
        None,
    );
}

#[test]
fn typed_deterministic_generation_records_exact_disposition_and_nonprogress() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    let mut pending = None;
    state.begin_model_generation_with_metadata(
        &mut pending,
        &SessionSource::Cli,
        Some(TurnTimingGenerationPurpose::Wait),
        TurnTimingGenerationDisposition::Deterministic,
        Some("trusted-state".to_string()),
    );
    drop(state.begin_model_request_wait());
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
                class: DeterministicContinuationClass::SourceCoverage,
                wire_identity: String::new(),
                resource_identity_hash: format!("resource-{index}"),
                state_revision: "revision".to_string(),
                host_action: DeterministicContinuationHostAction::ReuseCoveredSpan,
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
fn source_discovery_operations_boundaries_and_section_cells_are_independent() {
    let (_clock, state) = timing();
    state.mark_turn_started();
    state.record_source_discovery(SourceDiscoveryTimingEvent::Locator);
    state.record_source_discovery(SourceDiscoveryTimingEvent::Bundle {
        generation_micros: 17,
        materialized: [1, 2, 3, 4, 5, 6, 7],
        inline: [1, 1, 1, 1, 1, 1, 1],
        avoided_singleton_reads: 3,
    });
    state.record_source_discovery(SourceDiscoveryTimingEvent::SearchAfterOwner);
    state.record_source_discovery(SourceDiscoveryTimingEvent::Recovery);
    state.record_source_discovery(SourceDiscoveryTimingEvent::Recovery);
    state.record_source_discovery(SourceDiscoveryTimingEvent::DirectReadRequested);

    let counters = state
        .complete_snapshot()
        .protocol_timing()
        .counters
        .source_discovery;
    assert_eq!(counters.locator_operation_count, 1);
    assert_eq!(counters.bundle_operation_count, 1);
    assert_eq!(counters.search_operation_count, 1);
    assert_eq!(counters.recovery_operation_count, 2);
    assert_eq!(counters.read_operation_count, 1);
    assert_eq!(counters.locator_to_discovery_boundary_count, 1);
    assert_eq!(counters.discovery_to_discovery_boundary_count, 1);
    assert_eq!(counters.recovery_to_recovery_boundary_count, 1);
    assert_eq!(counters.searches_after_owner_count, 1);
    assert_eq!(counters.materialized_sections.primary_implementation, 1);
    assert_eq!(counters.materialized_sections.direct_callers, 2);
    assert_eq!(counters.inline_sections.primary_implementation, 1);
    assert_eq!(counters.avoided_singleton_read_count, 3);
    assert_eq!(counters.bundle_generation_micros, 17);
}

#[test]
fn pre_edit_convergence_records_fake_clock_milestones_and_bounded_counters() {
    let (clock, state) = timing();
    state.mark_turn_started();
    state.activate_pre_edit_convergence();

    let mut pending = None;
    state.begin_model_generation(&mut pending, &SessionSource::Cli);
    state.record_tool_call();
    clock.set_ms(10);
    state.record_pre_edit_owner_resolved();

    state.record_tool_call();
    clock.set_ms(20);
    state.record_pre_edit_implementation_ready();
    state.record_pre_edit_material_evidence();
    state.record_source_discovery(SourceDiscoveryTimingEvent::PostClosureSearch {
        has_question: false,
    });
    state.record_pre_edit_reopen(PreEditReopenReason::IncompleteEvidence);

    let mut pending = Some(ContinuationCause::ToolResult);
    state.begin_model_generation(&mut pending, &SessionSource::Cli);
    state.record_tool_call();
    clock.set_ms(30);
    state.record_pre_edit_first_accepted_mutation();
    clock.set_ms(35);
    state.record_pre_edit_first_successful_mutation();
    clock.set_ms(50);
    state.record_pre_edit_first_successful_mutation();

    let convergence = state
        .complete_snapshot()
        .protocol_timing()
        .pre_edit_convergence
        .expect("activated convergence timing");
    assert_eq!(convergence.owner_resolved_ms, Some(10));
    assert_eq!(convergence.accepted_to_owner_resolved_ms, Some(10));
    assert_eq!(convergence.owner_resolved_generation_count, Some(1));
    assert_eq!(convergence.owner_resolved_tool_call_count, Some(1));
    assert_eq!(convergence.implementation_ready_ms, Some(20));
    assert_eq!(convergence.implementation_ready_generation_count, Some(1));
    assert_eq!(convergence.implementation_ready_tool_call_count, Some(2));
    assert_eq!(convergence.owner_resolved_to_bundle_ready_ms, Some(10));
    assert_eq!(convergence.first_accepted_mutation_ms, Some(30));
    assert_eq!(
        convergence.bundle_ready_to_first_accepted_mutation_ms,
        Some(10)
    );
    assert!(!convergence.mutation_before_ready);
    assert_eq!(convergence.first_successful_mutation_ms, Some(35));
    assert_eq!(
        convergence.first_successful_mutation_generation_count,
        Some(2)
    );
    assert_eq!(
        convergence.first_successful_mutation_tool_call_count,
        Some(3)
    );
    assert_eq!(convergence.material_evidence_operations, 1);
    assert_eq!(convergence.broad_discovery_after_ready, 1);
    assert_eq!(convergence.readiness_reopen_count, 1);
    assert_eq!(convergence.reopen_reason_counts.incomplete_evidence, 1);
}

#[test]
fn accepted_mutation_before_bundle_ready_is_recorded_separately() {
    let (clock, state) = timing();
    state.mark_turn_started();
    state.activate_pre_edit_convergence();
    clock.set_ms(7);
    state.record_pre_edit_first_accepted_mutation();
    clock.set_ms(12);
    state.record_pre_edit_first_accepted_mutation();
    clock.set_ms(15);
    state.record_pre_edit_implementation_ready();

    let convergence = state
        .complete_snapshot()
        .protocol_timing()
        .pre_edit_convergence
        .expect("activated convergence timing");
    assert_eq!(convergence.first_accepted_mutation_ms, Some(7));
    assert!(convergence.mutation_before_ready);
    assert_eq!(convergence.bundle_ready_to_first_accepted_mutation_ms, None);
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

    let primary = state.begin_model_request_wait();
    clock.set_ms(10);
    drop(primary);
    state.record_tool_call();
    state.record_tool_call();

    state.record_model_retry();
    let retry = state.begin_model_request_wait();
    clock.set_ms(15);
    drop(retry);

    state.record_model_fallback();
    state.record_model_retry();
    let fallback = state.begin_model_request_wait();
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
    assert_eq!(profile.schema_version, 11);
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
fn parallel_latency_counters_are_additive() {
    let (_clock, state) = timing();

    state.record_wait_only_generation();
    state.record_internally_drained_waits(7);
    state.record_repeated_discovery_call();
    state.record_discovery_after_owner_resolution();
    state.record_no_progress_directive();
    state.record_proven_loop_activation();
    state.record_tool_output_projection(100);
    state.record_tool_output_projection(50);
    state.record_tool_output_projection_facts(1_000, 250, 400, 100, true, true, 3);
    state.record_tool_output_projection_facts(500, 125, 200, 50, false, false, 1);
    state.record_tool_output_artifact_reread();
    state.record_tool_output_recovery(2);
    state.record_search_index(true);
    state.record_search_index(false);
    state.record_strict_subset_source_reread();
    state.record_truncation_induced_continuation();

    let counters = state.complete_snapshot().protocol_timing().counters;
    assert_eq!(counters.wait_only_generation_count, 1);
    assert_eq!(counters.internally_drained_wait_count, 7);
    assert_eq!(counters.suppressed_deterministic_continuation_count, 0);
    assert_eq!(counters.suppressed_repeated_dispatch_count, 0);
    assert_eq!(counters.repeated_discovery_call_count, 1);
    assert_eq!(counters.discovery_after_owner_resolution_count, 1);
    assert_eq!(counters.no_progress_directive_count, 1);
    assert_eq!(counters.proven_loop_activation_count, 1);
    assert_eq!(counters.tool_output_truncation_count, 2);
    assert_eq!(counters.tool_output_projected_token_count, 150);
    assert_eq!(counters.tool_output_artifact_reread_count, 1);
    assert_eq!(counters.tool_output_canonical_byte_count, 1_500);
    assert_eq!(counters.tool_output_canonical_token_count, 375);
    assert_eq!(counters.tool_output_model_byte_count, 600);
    assert_eq!(counters.tool_output_model_token_count, 150);
    assert_eq!(counters.tool_output_artifact_creation_count, 1);
    assert_eq!(counters.tool_output_projection_truncation_count, 1);
    assert_eq!(counters.tool_output_omitted_section_count, 4);
    assert_eq!(counters.tool_output_recovery_call_count, 1);
    assert_eq!(counters.tool_output_recovery_retruncation_count, 2);
    assert_eq!(counters.tool_output_recursive_spill_count, 0);
    assert_eq!(counters.strict_subset_source_reread_count, 1);
    assert_eq!(counters.complete_search_index_count, 1);
    assert_eq!(counters.incomplete_search_index_count, 1);
    assert_eq!(counters.truncation_induced_continuation_count, 1);
}

#[test]
fn projected_output_counts_only_the_next_tool_result_generation_as_recovery() {
    let (_clock, state) = timing();

    state.record_tool_output_projection(40);
    state.record_tool_output_projection(20);
    let mut pending = Some(ContinuationCause::ToolResult);
    state.begin_model_generation(&mut pending, &SessionSource::Cli);

    state.record_tool_output_projection(10);
    let mut pending = Some(ContinuationCause::PendingInput);
    state.begin_model_generation(&mut pending, &SessionSource::Cli);

    state.record_tool_output_projection(5);
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
