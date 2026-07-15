use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_analytics::TurnProfile;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;

use super::AGENT_MESSAGE_SETTLED;
use super::ClockSample;
use super::InteractiveWaitKind;
use super::MODEL_OUTPUT_SETTLED;
use super::ModelStreamTimingGuard;
use super::TimeSample;
use super::TurnClock;
use super::TurnLocalPhase;
use super::TurnTimingState;
use super::VISIBLE_OUTPUT_SETTLED;
use super::response_item_records_model_output;
use super::response_item_records_visible_output;
use crate::ResponseEvent;

const NS_PER_MS: u128 = 1_000_000;

#[derive(Debug)]
struct FakeClock {
    sample: Mutex<TimeSample>,
    samples: AtomicUsize,
}

impl FakeClock {
    fn new(monotonic_ns: u128, wall_unix_ms: i64) -> Self {
        Self {
            sample: Mutex::new(TimeSample {
                monotonic_ns,
                wall_unix_ms,
            }),
            samples: AtomicUsize::new(0),
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

    fn sample_count(&self) -> usize {
        self.samples.load(Ordering::SeqCst)
    }
}

impl TurnClock for FakeClock {
    fn sample(&self) -> ClockSample {
        self.samples.fetch_add(1, Ordering::SeqCst);
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

fn agent_message(id: &str) -> TurnItem {
    TurnItem::AgentMessage(AgentMessageItem {
        id: id.to_string(),
        content: Vec::new(),
        phase: None,
        memory_citation: None,
    })
}

#[test]
fn model_stream_timing_guard_closes_once_for_all_exit_styles() {
    for exit_style in ["success", "error", "eof", "cancellation"] {
        let (clock, state) = timing();
        state.mark_turn_started();
        let mut guard = ModelStreamTimingGuard::new(Some(&state));
        guard.begin_wait();
        clock.set_ms(5);
        guard.begin_processing();
        clock.set_ms(12);

        if exit_style == "success" {
            guard.finish();
            guard.finish();
        }
        drop(guard);
        clock.set_ms(50);

        let profile = state.complete_snapshot().profile;
        assert_eq!(
            profile.unions.model_stream_wait_ns,
            5 * NS_PER_MS,
            "{exit_style}"
        );
        assert_eq!(
            profile.unions.model_stream_processing_ns,
            7 * NS_PER_MS,
            "{exit_style}"
        );
        assert_eq!(profile.counters.invalid_transition_count, 0, "{exit_style}");
    }
}

#[test]
fn milestone_bits_publish_only_after_timestamps_are_committed() {
    let (clock, state) = timing();
    state.mark_turn_started();

    clock.set_ms(5);
    let (model_duration, model_bits) =
        state.commit_response_event_milestones(&ResponseEvent::ToolCallInputDelta {
            item_id: "tool-item".to_string(),
            call_id: Some("tool-call".to_string()),
            delta: "{}".to_string(),
        });
    assert_eq!(model_duration, None);
    assert_eq!(model_bits, MODEL_OUTPUT_SETTLED);
    assert_eq!(state.milestone_mask.load(Ordering::Acquire), 0);
    assert_eq!(
        state.state().milestones.first_model_output_ns,
        Some(5 * NS_PER_MS)
    );
    state.publish_milestones(model_bits);
    assert_eq!(
        state.milestone_mask.load(Ordering::Acquire),
        MODEL_OUTPUT_SETTLED
    );

    clock.set_ms(10);
    let (visible_duration, visible_bits) = state
        .commit_response_event_milestones(&ResponseEvent::OutputTextDelta("visible".to_string()));
    assert_eq!(visible_duration, Some(Duration::from_millis(10)));
    assert_eq!(visible_bits, VISIBLE_OUTPUT_SETTLED);
    assert_eq!(
        state.milestone_mask.load(Ordering::Acquire),
        MODEL_OUTPUT_SETTLED
    );
    assert_eq!(
        state.state().milestones.first_visible_output_ns,
        Some(10 * NS_PER_MS)
    );
    state.publish_milestones(visible_bits);

    clock.set_ms(15);
    let (agent_duration, agent_bits) =
        state.commit_agent_message_milestone(&agent_message("agent-1"));
    assert_eq!(agent_duration, Some(Duration::from_millis(15)));
    assert_eq!(agent_bits, AGENT_MESSAGE_SETTLED);
    assert_eq!(
        state.milestone_mask.load(Ordering::Acquire),
        MODEL_OUTPUT_SETTLED | VISIBLE_OUTPUT_SETTLED
    );
    assert_eq!(
        state.state().milestones.first_agent_message_ns,
        Some(15 * NS_PER_MS)
    );
    state.publish_milestones(agent_bits);
    assert_eq!(
        state.milestone_mask.load(Ordering::Acquire),
        MODEL_OUTPUT_SETTLED | VISIBLE_OUTPUT_SETTLED | AGENT_MESSAGE_SETTLED
    );
}

#[test]
fn existing_milestones_are_published_by_following_recorders() {
    let (clock, model_state) = timing();
    model_state.mark_turn_started();
    clock.set_ms(5);
    let model_event = ResponseEvent::ToolCallInputDelta {
        item_id: "tool-item".to_string(),
        call_id: Some("tool-call".to_string()),
        delta: "{}".to_string(),
    };
    let (_, model_bits) = model_state.commit_response_event_milestones(&model_event);
    assert_eq!(model_bits, MODEL_OUTPUT_SETTLED);
    assert_eq!(
        model_state.record_response_event_milestones(&model_event),
        None
    );
    assert_ne!(
        model_state.milestone_mask.load(Ordering::Acquire) & MODEL_OUTPUT_SETTLED,
        0
    );

    let (clock, visible_state) = timing();
    visible_state.mark_turn_started();
    clock.set_ms(5);
    let visible_event = ResponseEvent::OutputTextDelta("visible".to_string());
    let (_, visible_bits) = visible_state.commit_response_event_milestones(&visible_event);
    assert_eq!(visible_bits, MODEL_OUTPUT_SETTLED | VISIBLE_OUTPUT_SETTLED);
    assert_eq!(
        visible_state.record_response_event_milestones(&visible_event),
        None
    );
    assert_eq!(
        visible_state.milestone_mask.load(Ordering::Acquire)
            & (MODEL_OUTPUT_SETTLED | VISIBLE_OUTPUT_SETTLED),
        MODEL_OUTPUT_SETTLED | VISIBLE_OUTPUT_SETTLED
    );

    let (clock, agent_state) = timing();
    agent_state.mark_turn_started();
    clock.set_ms(5);
    let item = agent_message("agent-1");
    let (_, agent_bits) = agent_state.commit_agent_message_milestone(&item);
    assert_eq!(agent_bits, AGENT_MESSAGE_SETTLED);
    assert_eq!(agent_state.record_ttfm_for_turn_item(&item), None);
    assert_ne!(
        agent_state.milestone_mask.load(Ordering::Acquire) & AGENT_MESSAGE_SETTLED,
        0
    );
}

#[test]
fn settled_milestones_skip_repeated_clock_sampling() {
    let (clock, state) = timing();
    state.mark_turn_started();
    assert_eq!(clock.sample_count(), 1);

    clock.set_ms(5);
    let visible_event = ResponseEvent::OutputTextDelta("visible".to_string());
    assert_eq!(
        state.record_response_event_milestones(&visible_event),
        Some(Duration::from_millis(5))
    );
    assert_eq!(clock.sample_count(), 2);
    assert_eq!(state.record_response_event_milestones(&visible_event), None);
    assert_eq!(clock.sample_count(), 2);

    clock.set_ms(10);
    let item = agent_message("agent-1");
    assert_eq!(
        state.record_ttfm_for_turn_item(&item),
        Some(Duration::from_millis(10))
    );
    assert_eq!(clock.sample_count(), 3);
    assert_eq!(state.record_ttfm_for_turn_item(&item), None);
    assert_eq!(clock.sample_count(), 3);
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
        }
    );
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
    assert_eq!(profile.schema_version, 1);
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
